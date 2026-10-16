use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};
use trakkin_mappings_corpus::{
    Artifact, ConflictPolicy,
    git::History,
    pipeline::{Action, Database, Review},
};

#[derive(Args)]
pub struct Command {
    #[arg(long, global = true)]
    database_url: Option<String>,
    #[command(subcommand)]
    operation: Operation,
}

#[derive(Subcommand)]
enum Operation {
    /// Canonicalize DSL without requiring a database.
    Fmt {
        #[arg(default_value = "-")]
        file: PathBuf,
        #[arg(long)]
        check: bool,
    },
    /// Update projections, evidence, and dependent native episode suggestions.
    Sync {
        #[arg(long, default_value_t = 1000)]
        batch_size: usize,
        #[arg(long)]
        max_pages: Option<usize>,
        #[arg(long)]
        policy: Option<PathBuf>,
    },
    /// Inspect consumer progress, work queues, and publication state.
    Status,
    /// List candidate records, current evidence versions, and decisions.
    Candidates {
        #[arg(long)]
        after: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Inspect evidence and review history for a claim.
    Show {
        claim_id: String,
        #[arg(long)]
        after_support: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Record an explicit version-checked review or authored DSL decision.
    Decide {
        #[arg(required_unless_present = "file", conflicts_with = "file")]
        claim_id: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long, value_enum)]
        action: Decision,
        #[arg(long)]
        note: String,
        #[arg(long)]
        expected_candidate: Option<i64>,
        #[arg(long)]
        expected_decision: Option<i64>,
        #[arg(long, requires = "replaces_version")]
        replaces: Option<String>,
        #[arg(long, requires = "replaces")]
        replaces_version: Option<i64>,
    },
    /// Apply pending decisions to a managed Git publication repository.
    Publish {
        #[arg(long)]
        repository: PathBuf,
        #[arg(long, default_value_t = 250)]
        batch_size: usize,
        #[arg(long)]
        remote: Option<String>,
        #[arg(long, default_value = "mappings")]
        branch: String,
    },
    /// Validate canonical DSL files in a published Git repository.
    Validate {
        #[arg(long)]
        repository: PathBuf,
    },
    /// Remove a bounded page of unreferenced old source versions and publications.
    Retain {
        #[arg(long, default_value_t = 30)]
        keep_days: i32,
        #[arg(long, default_value_t = 100)]
        keep_publications: i64,
        #[arg(long, default_value_t = 1000)]
        batch_size: usize,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Decision {
    Accepted,
    Rejected,
    Retracted,
}

impl Command {
    pub fn run(self) -> Result<()> {
        match self.operation {
            Operation::Fmt { file, check } => {
                let text = read(&file)?;
                let canonical = trakkin_mappings_language::parse(&text)?
                    .iter()
                    .map(|record| record.canonical())
                    .collect::<String>();
                if check {
                    ensure!(text == canonical, "DSL is not canonical");
                } else if file == Path::new("-") {
                    print!("{canonical}");
                } else {
                    fs::write(file, canonical)?;
                }
                Ok(())
            }
            Operation::Validate { repository } => {
                ensure!(repository.exists(), "publication repository does not exist");
                let runtime = tokio::runtime::Runtime::new()?;
                let repository = runtime.block_on(History::open(&repository))?;
                let commit = runtime.block_on(repository.head())?;
                runtime.block_on(repository.policy(&commit))?;
                let mut delta = runtime.block_on(repository.delta(None, &commit))?;
                let mut records = 0;
                loop {
                    let batch = runtime.block_on(delta.next_batch(1000))?;
                    if batch.is_empty() {
                        break;
                    }
                    for change in batch {
                        if change.path != trakkin_mappings_corpus::POLICY {
                            Artifact::parse(
                                &change.path,
                                change.text.as_deref().context("missing published file")?,
                            )?;
                            records += 1;
                        }
                    }
                }
                output(&json!({"commit":commit,"records":records}))
            }
            operation => {
                let url = self
                    .database_url
                    .or_else(|| std::env::var("TRAKKIN_MAPPINGS_DATABASE_URL").ok())
                    .context("set TRAKKIN_MAPPINGS_DATABASE_URL or --database-url")?;
                let mut database = Database::connect(&url)?;
                match operation {
                    Operation::Sync {
                        batch_size,
                        max_pages,
                        policy,
                    } => {
                        if let Some(path) = policy {
                            database
                                .set_policy(&ConflictPolicy::parse(&fs::read_to_string(path)?)?)?;
                        }
                        output(&serde_json::to_value(
                            database.sync(batch_size, max_pages)?,
                        )?)
                    }
                    Operation::Status => output(&database.status()?),
                    Operation::Candidates { after, limit } => output(&serde_json::to_value(
                        database.candidates(after.as_deref(), limit)?,
                    )?),
                    Operation::Show {
                        claim_id,
                        after_support,
                        limit,
                    } => output(&database.show(&claim_id, after_support.as_deref(), limit)?),
                    Operation::Decide {
                        claim_id,
                        file,
                        action,
                        note,
                        expected_candidate,
                        expected_decision,
                        replaces,
                        replaces_version,
                    } => {
                        let record = if let Some(file) = file {
                            read(&file)?
                        } else {
                            ensure!(
                                expected_candidate.is_some()
                                    || matches!(action, Decision::Retracted),
                                "candidate review requires --expected-candidate"
                            );
                            let shown =
                                database.show(&claim_id.context("missing claim")?, None, 1)?;
                            shown["record"]
                                .as_str()
                                .or_else(|| shown["decision"]["record"].as_str())
                                .context("claim has no record")?
                                .to_owned()
                        };
                        let version = database.review(Review {
                            record,
                            action: match action {
                                Decision::Accepted => Action::Accepted,
                                Decision::Rejected => Action::Rejected,
                                Decision::Retracted => Action::Retracted,
                            },
                            note,
                            expected_candidate,
                            expected_decision,
                            replaces: replaces.zip(replaces_version),
                        })?;
                        output(&json!({"decision_version":version}))
                    }
                    Operation::Publish {
                        repository,
                        batch_size,
                        remote,
                        branch,
                    } => {
                        let runtime = tokio::runtime::Runtime::new()?;
                        let repository = runtime.block_on(History::open(&repository))?;
                        let report = if let Some(remote) = remote {
                            database.publish_to(
                                &repository,
                                &runtime,
                                batch_size,
                                &remote,
                                &branch,
                            )?
                        } else {
                            database.publish(&repository, &runtime, batch_size)?
                        };
                        output(&serde_json::to_value(report)?)
                    }
                    Operation::Retain {
                        keep_days,
                        keep_publications,
                        batch_size,
                    } => output(&serde_json::to_value(database.retain(
                        keep_days,
                        keep_publications,
                        batch_size,
                    )?)?),
                    Operation::Fmt { .. } | Operation::Validate { .. } => unreachable!(),
                }
            }
        }
    }
}

fn read(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        let mut text = String::new();
        io::stdin().read_to_string(&mut text)?;
        Ok(text)
    } else {
        Ok(fs::read_to_string(path)?)
    }
}
fn output(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
