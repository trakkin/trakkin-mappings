use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use indicatif::{ProgressBar, ProgressStyle};
use serde_json::json;
use trakkin_mappings_source::{
    Phase, Report, Source, SyncOptions,
    sources::{AniList, Tmdb, Tvdb},
    storage::Postgres,
};

#[derive(Args)]
pub struct Command {
    /// PostgreSQL connection URL; defaults to TRAKKIN_MAPPINGS_DATABASE_URL.
    #[arg(long, global = true, value_name = "URL")]
    database_url: Option<String>,
    #[command(subcommand)]
    action: Action,
}

#[derive(Clone, Copy, ValueEnum)]
enum SourceKind {
    #[value(name = "co.anilist")]
    Anilist,
    #[value(name = "org.themoviedb")]
    Tmdb,
    #[value(name = "com.thetvdb")]
    Tvdb,
}

impl SourceKind {
    fn namespace(self) -> &'static str {
        match self {
            Self::Anilist => "co.anilist",
            Self::Tmdb => "org.themoviedb",
            Self::Tvdb => "com.thetvdb",
        }
    }

    fn domains(self, selected: Option<&str>) -> Result<Vec<&'static str>> {
        let domains = match self {
            Self::Anilist => AniList::DOMAINS,
            Self::Tmdb => Tmdb::DOMAINS,
            Self::Tvdb => Tvdb::DOMAINS,
        };
        let Some(selected) = selected else {
            return Ok(domains.to_vec());
        };
        let domain = domains
            .iter()
            .copied()
            .find(|domain| *domain == selected)
            .with_context(|| {
                format!(
                    "Unsupported domain {selected} for {}; choose {}",
                    self.namespace(),
                    domains.join(", ")
                )
            })?;
        Ok(vec![domain])
    }

    fn open(self, domains: &[&str]) -> Result<Box<dyn Source>> {
        Ok(match self {
            Self::Anilist => Box::new(AniList::new()?),
            Self::Tmdb => Box::new(Tmdb::new(
                std::env::var("TRAKKIN_MAPPINGS_INGESTION_TMDB_TOKEN")
                    .context("Set TRAKKIN_MAPPINGS_INGESTION_TMDB_TOKEN")?,
                domains,
            )?),
            Self::Tvdb => Box::new(Tvdb::new(
                std::env::var("TRAKKIN_MAPPINGS_INGESTION_TVDB_API_KEY")
                    .context("Set TRAKKIN_MAPPINGS_INGESTION_TVDB_API_KEY")?,
                std::env::var("TRAKKIN_MAPPINGS_INGESTION_TVDB_PIN").ok(),
                domains,
            )?),
        })
    }
}

#[derive(Subcommand)]
enum Action {
    /// Synchronize a source, initializing or resuming it automatically.
    Sync {
        #[arg(value_enum)]
        source: SourceKind,
        /// Refresh the complete catalog and reconcile missing records.
        #[arg(long)]
        full: bool,
        /// Replace unfinished progress without deleting committed records or the successful baseline.
        #[arg(long)]
        restart: bool,
        /// Select one domain; omit to synchronize all source domains.
        #[arg(long)]
        domain: Option<String>,
        #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..=1000))]
        batch_size: u32,
    },
    /// Show synchronization state for every saved domain selection.
    Status {
        #[arg(value_enum)]
        source: SourceKind,
    },
    /// Read one native record, including tombstones and acquisition metadata.
    Show {
        #[arg(value_enum)]
        source: SourceKind,
        key: String,
    },
}

impl Command {
    pub fn run(self) -> Result<()> {
        let (source, domains) = match &self.action {
            Action::Sync { source, domain, .. } => (*source, source.domains(domain.as_deref())?),
            Action::Status { source } => (*source, source.domains(None)?),
            Action::Show { source, key } => {
                let (domain, _) = key
                    .split_once(':')
                    .filter(|(_, native)| !native.is_empty())
                    .context("Use a qualified native key such as episode:12345")?;
                (*source, source.domains(Some(domain))?)
            }
        };
        let url = self
            .database_url
            .or_else(|| std::env::var("TRAKKIN_MAPPINGS_DATABASE_URL").ok())
            .filter(|url| !url.trim().is_empty())
            .context("Set --database-url or TRAKKIN_MAPPINGS_DATABASE_URL")?;
        let mut storage = Postgres::connect(&url, source.namespace(), &domains)?;
        let result = match self.action {
            Action::Sync {
                full,
                restart,
                batch_size,
                ..
            } => {
                let mut upstream = source.open(&domains)?;
                let progress = SyncProgress::new(source.namespace());
                let run = trakkin_mappings_source::sync(
                    &mut *upstream,
                    &mut storage,
                    SyncOptions {
                        full,
                        restart,
                        batch_size: batch_size as usize,
                    },
                    chrono::Utc::now().timestamp(),
                    |phase, report| progress.update(phase, report),
                )?;
                json!({"source":source.namespace(),"domains":domains,"revision":storage.revision()?,"run":run})
            }
            Action::Status { .. } => {
                serde_json::to_value(trakkin_mappings_source::status(&mut storage)?)?
            }
            Action::Show { key, .. } => {
                let record = storage
                    .get(&key)?
                    .with_context(|| format!("No record {key} in {}", source.namespace()))?;
                json!({"source":source.namespace(),"record":record})
            }
        };
        println!("{}", serde_json::to_string_pretty(&result)?);
        Ok(())
    }
}

struct SyncProgress {
    bar: ProgressBar,
}

impl SyncProgress {
    fn new(source: &str) -> Self {
        let bar = ProgressBar::new_spinner();
        bar.set_style(
            ProgressStyle::with_template("{spinner} {prefix} [{elapsed_precise}] {wide_msg}")
                .expect("valid progress template")
                .tick_strings(&["-", "\\", "|", "/", " "]),
        );
        bar.set_prefix(source.to_owned());
        bar.enable_steady_tick(std::time::Duration::from_millis(100));
        Self { bar }
    }

    fn update(&self, phase: Phase, report: &Report) {
        self.bar.set_message(format!(
            "{phase} | discovered {} | fetched {} | changed {}",
            report.discovered, report.fetched, report.changed
        ));
    }
}

impl Drop for SyncProgress {
    fn drop(&mut self) {
        self.bar.finish_and_clear();
    }
}
