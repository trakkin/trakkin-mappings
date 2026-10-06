use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use indicatif::{ProgressBar, ProgressStyle};
use std::{fs::OpenOptions, path::PathBuf};
use trakkin_mappings_ingestion::{
    Operation, Provider,
    providers::AniList,
    storage::{Paimon, RoutedStorage, Storage},
};

#[derive(Args)]
pub struct Command {
    #[arg(long, global = true)]
    warehouse: Option<String>,
    #[arg(long, global = true)]
    /// Select one domain; omit to process all domains for the provider.
    domain: Option<String>,
    #[arg(long, value_enum, global = true)]
    provider: Option<Source>,
    #[arg(
        long,
        default_value = "crates/trakkin-mappings-ingestion/java/target",
        global = true
    )]
    bridge: PathBuf,
    /// Override the writer lock, which defaults to writer.lock in the bridge directory.
    #[arg(long, global = true)]
    lock_file: Option<PathBuf>,
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..=1000), global = true)]
    batch_size: u32,
    #[command(subcommand)]
    operation: Action,
}

#[derive(Clone, Copy, ValueEnum)]
enum Source {
    #[value(name = "co.anilist")]
    Anilist,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Self::Anilist => "co.anilist",
        }
    }
    fn open(self) -> Result<Box<dyn Provider>> {
        Ok(match self {
            Self::Anilist => Box::new(AniList::new()?),
        })
    }
}

#[derive(Subcommand)]
enum Action {
    /// Populate missing records and establish the initial synchronization watermark.
    Bootstrap,
    /// Acquire changed records from the committed watermark.
    Sync,
    /// Refresh every catalogue record and confirm absences before tombstoning.
    Reconcile,
    /// Compact files, expire snapshots, and clean old orphan files.
    Maintain,
    /// Read all current records and verify payload hashes.
    Validate,
    /// Browse native records, tombstones, and committed checkpoints without upstream API calls.
    Inspect {
        #[arg(long)]
        key: Option<String>,
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=1000))]
        limit: u32,
        #[arg(long)]
        include_deleted: bool,
        /// Also scan the full key/hash index to count live records and tombstones.
        #[arg(long)]
        summary: bool,
    },
}

impl Command {
    pub fn run(self) -> Result<()> {
        let warehouse = self
            .warehouse
            .or_else(|| std::env::var("TRAKKIN_MAPPINGS_INGESTION_WAREHOUSE").ok())
            .context("Set --warehouse or TRAKKIN_MAPPINGS_INGESTION_WAREHOUSE")?;
        let provider = self.provider.context("Set --provider")?;
        let domains = trakkin_mappings_ingestion::dataset::select_domains(
            provider.name(),
            self.domain.as_deref(),
        )?;
        let warehouses = domains
            .iter()
            .map(|domain| {
                trakkin_mappings_ingestion::dataset::warehouse(&warehouse, provider.name(), domain)
            })
            .collect::<Result<Vec<_>>>()?;
        let lock_file = self
            .lock_file
            .unwrap_or_else(|| self.bridge.join("writer.lock"));
        if let Some(parent) = lock_file
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_file)
            .with_context(|| format!("Open ingestion writer lock {}", lock_file.display()))?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .context("Another local mirror operation holds the writer lock")?;
        let job_warehouse = trakkin_mappings_ingestion::dataset::job_warehouse(
            &warehouse,
            provider.name(),
            &domains,
        )?;
        if let Some(operation) = match self.operation {
            Action::Bootstrap => Some(Operation::Bootstrap),
            Action::Sync => Some(Operation::Sync),
            Action::Reconcile => Some(Operation::Reconcile),
            _ => None,
        } {
            let progress = LiveProgress::new(provider.name(), &domains.join("+"));
            progress.0.set_message("opening acquisition job");
            let coordinator = Paimon::open(&self.bridge, &job_warehouse)?;
            let datasets = domains
                .iter()
                .zip(&warehouses)
                .map(|(domain, warehouse)| {
                    Ok(((*domain).to_owned(), Paimon::open(&self.bridge, warehouse)?))
                })
                .collect::<Result<_>>()?;
            let mut storage = RoutedStorage::new(coordinator, datasets)?;
            let mut plan = trakkin_mappings_ingestion::dataset::AcquisitionPlan::new(
                provider.open()?,
                &domains,
            )?;
            let report = trakkin_mappings_ingestion::run(
                &mut plan,
                &mut storage,
                operation,
                self.batch_size as usize,
                chrono::Utc::now().timestamp(),
                |phase, report| {
                    progress.0.set_message(format!(
                        "{phase} | discovered {} | fetched {} | changed {}",
                        report.discovered, report.fetched, report.changed
                    ));
                },
            )?;
            let mut snapshots = serde_json::Map::new();
            for (domain, dataset) in storage.datasets_mut() {
                snapshots.insert(domain.clone(), dataset.snapshot_id()?);
            }
            progress.0.finish_and_clear();
            println!(
                "{}",
                serde_json::json!({"provider":provider.name(),"domains":domains,"snapshots":snapshots,"report":report})
            );
            return Ok(());
        }
        let mut coordinator = Paimon::open(&self.bridge, &job_warehouse)?;
        match self.operation {
            Action::Validate => coordinator.validate()?,
            Action::Maintain => {
                coordinator.maintain()?;
                coordinator.validate()?;
            }
            _ => {}
        }
        for (domain, warehouse) in domains.iter().zip(&warehouses) {
            let progress = LiveProgress::new(provider.name(), domain);
            progress.0.set_message("opening storage");
            let mut storage = Paimon::open(&self.bridge, warehouse)?;
            match &self.operation {
                Action::Inspect {
                    key,
                    limit,
                    include_deleted,
                    summary,
                } => {
                    let records =
                        storage.inspect(key.as_deref(), *limit as usize, *include_deleted)?;
                    let mut checkpoints = serde_json::Map::new();
                    for operation in ["bootstrap", "sync", "reconcile"] {
                        checkpoints.insert(operation.into(), coordinator.checkpoint(operation)?);
                    }
                    let mut result = serde_json::json!({"provider":provider.name(),"domain":domain,"records":records,"checkpoints":checkpoints});
                    result["snapshot_id"] = storage.snapshot_id()?;
                    if *summary {
                        let index = storage.index()?;
                        let deleted = index.values().filter(|entry| entry.deleted).count();
                        result["counts"] =
                            serde_json::json!({"live":index.len()-deleted,"deleted":deleted});
                    }
                    progress.0.finish_and_clear();
                    println!("{}", serde_json::to_string_pretty(&result)?);
                }
                Action::Maintain => {
                    progress.0.set_message("compacting and cleaning storage");
                    storage.maintain()?;
                    progress.0.set_message("validating");
                    storage.validate()?;
                    progress.0.finish_and_clear();
                    println!(
                        "{} / {domain}: maintenance and integrity validation complete",
                        provider.name()
                    );
                }
                Action::Validate => {
                    progress.0.set_message("validating");
                    storage.validate()?;
                    progress.0.finish_and_clear();
                    println!(
                        "{} / {domain}: integrity validation complete",
                        provider.name()
                    );
                }
                _ => unreachable!(),
            }
        }
        Ok(())
    }
}

struct LiveProgress(ProgressBar);

impl LiveProgress {
    fn new(source: &str, domain: &str) -> Self {
        let bar = ProgressBar::new_spinner();
        bar.set_style(
            ProgressStyle::with_template("{spinner} {prefix} [{elapsed_precise}] {wide_msg}")
                .expect("valid progress template")
                .tick_strings(&["-", "\\", "|", "/", " "]),
        );
        bar.set_prefix(format!("{source}/{domain}"));
        bar.enable_steady_tick(std::time::Duration::from_millis(100));
        Self(bar)
    }
}

impl Drop for LiveProgress {
    fn drop(&mut self) {
        self.0.finish_and_clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Arguments {
        #[command(flatten)]
        command: Command,
    }
}
