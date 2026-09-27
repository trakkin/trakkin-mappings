use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use std::{
    fs,
    io::{self, Read},
    path::PathBuf,
};
use trakkin_mappings_corpus::{self as corpus, adapters::Adapters};

#[derive(Parser)]
#[command(version, about = "Maintain the canonical Trakkin mapping corpus")]
struct Cli {
    #[arg(long, global = true, default_value = ".")]
    root: PathBuf,
    #[arg(long, global = true, default_value = corpus::ADAPTERS)]
    adapters: PathBuf,
    #[command(subcommand)]
    command: Operation,
}

#[derive(Subcommand)]
enum Operation {
    /// Canonicalize a DSL document; stdin is used when FILE is omitted or '-'.
    Fmt {
        file: Option<PathBuf>,
        #[arg(long)]
        check: bool,
        /// Also normalize and check canonical-corpus annotation rules.
        #[arg(long)]
        corpus: bool,
    },
    /// Print the stable ID, shard, and canonical statement for one mapping.
    Locate { mapping: String },
    /// Insert a document; stdin is used when FILE is omitted or '-'.
    Insert {
        file: Option<PathBuf>,
        #[arg(long)]
        replace_metadata: bool,
    },
    /// Remove a mapping by its full ID; remove its shard if it becomes empty.
    Remove { id: String },
    /// Validate the corpus, or committed changes since BASE through HEAD.
    Validate {
        #[arg(long)]
        base: Option<String>,
        /// Also reconcile the cached index and check cross-shard conflicts.
        #[arg(long)]
        indexed: bool,
    },
    /// Incrementally build a derived SQLite query/conflict index.
    Index {
        #[arg(long, default_value = corpus::artifacts::INDEX)]
        output: PathBuf,
    },
    /// Look up mappings by an exact opaque reference in a derived SQLite database.
    Query {
        reference: String,
        #[arg(long, default_value = corpus::artifacts::INDEX)]
        database: PathBuf,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Produce verified SQLite/text release assets; output must not already exist.
    Release {
        #[arg(long, default_value = "dist/release")]
        output: PathBuf,
        #[arg(long)]
        allow_dirty: bool,
        #[arg(long, default_value_t = 1_900_000_000)]
        part_bytes: u64,
    },
    /// Report shard distribution and storage migration warnings as JSON.
    Stats,
}

fn read_input(file: Option<PathBuf>) -> Result<String> {
    if let Some(file) = file.filter(|path| path.as_os_str() != "-") {
        return Ok(fs::read_to_string(file)?);
    }
    let mut text = String::new();
    io::stdin().read_to_string(&mut text)?;
    Ok(text)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Operation::Fmt {
            file,
            check,
            corpus,
        } => {
            let input = read_input(file)?;
            let mut canonical = String::new();
            for record in trakkin_mappings_language::parse(&input)? {
                let record = if corpus {
                    record.corpus_form()?
                } else {
                    record
                };
                canonical.push_str(&record.canonical());
            }
            if check {
                ensure!(input == canonical, "document is not canonical");
            } else {
                print!("{canonical}");
            }
        }
        Operation::Locate { mapping } => {
            let record = corpus::single_record(&mapping)?;
            let id = record.statement.id();
            println!(
                "{}",
                serde_json::json!({ "id": id, "shard": corpus::shard_path(&id)?, "statement": record.statement.canonical() })
            );
        }
        Operation::Stats => println!(
            "{}",
            serde_json::to_string_pretty(&corpus::statistics(&cli.root)?)?
        ),
        Operation::Query {
            reference,
            database,
            limit,
        } => println!(
            "{}",
            serde_json::to_string_pretty(&trakkin_mappings_index::query(
                &cli.root.join(database),
                &reference,
                limit
            )?)?
        ),
        operation => {
            let adapters = Adapters::load(&cli.root.join(cli.adapters))?;
            match operation {
                Operation::Insert {
                    file,
                    replace_metadata,
                } => println!(
                    "{} record(s) changed",
                    corpus::insert(&cli.root, &read_input(file)?, &adapters, replace_metadata)?
                ),
                Operation::Remove { id } => {
                    println!("removed: {}", corpus::remove(&cli.root, &id, &adapters)?)
                }
                Operation::Validate { base, indexed } => {
                    let paths = if let Some(base) = base {
                        corpus::changed_shards(&cli.root, &base)?
                    } else {
                        corpus::inventory(&cli.root)?
                    };
                    println!(
                        "validated {} records in {} shards",
                        corpus::validate_paths(&cli.root, &paths, &adapters)?,
                        paths.len()
                    );
                    if indexed {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&corpus::artifacts::index(
                                &cli.root,
                                &cli.root.join(corpus::artifacts::INDEX),
                                &adapters
                            )?)?
                        );
                    }
                }
                Operation::Index { output } => println!(
                    "{}",
                    serde_json::to_string_pretty(&corpus::artifacts::index(
                        &cli.root,
                        &cli.root.join(output),
                        &adapters
                    )?)?
                ),
                Operation::Release {
                    output,
                    allow_dirty,
                    part_bytes,
                } => {
                    let (commit, dirty) =
                        corpus::artifacts::git_provenance(&cli.root, allow_dirty)?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&corpus::artifacts::release(
                            &cli.root,
                            &cli.root.join(output),
                            &adapters,
                            &commit,
                            dirty,
                            part_bytes
                        )?)?
                    );
                }
                _ => unreachable!(),
            }
        }
    }
    Ok(())
}
