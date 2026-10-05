use crate::storage::{Batch, Commit, Entry, Postgres, Update};
use crate::{Change, Content, FetchRequest, Mode, Resume, Source};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Default, Debug, Deserialize, Serialize)]
pub struct Report {
    pub discovered: u64,
    pub fetched: u64,
    pub changed: u64,
    pub complete: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct SyncOptions {
    pub full: bool,
    pub restart: bool,
    pub batch_size: usize,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            full: false,
            restart: false,
            batch_size: 1000,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub source: String,
    pub revision: i64,
    pub scopes: Vec<ScopeStatus>,
}

#[derive(Debug, Serialize)]
pub struct ScopeStatus {
    pub domains: Vec<String>,
    pub initialized: bool,
    pub watermark: Option<i64>,
    pub active: Option<RunStatus>,
    pub last_run: Option<RunSummary>,
}

#[derive(Debug, Serialize)]
pub struct RunStatus {
    pub id: String,
    pub mode: Mode,
    pub started_at: i64,
    pub progress: RunProgress,
    pub report: Report,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunSummary {
    pub id: String,
    pub mode: Mode,
    pub started_at: i64,
    pub completed_at: i64,
    pub report: Report,
}

pub fn status(storage: &mut Postgres) -> Result<Status> {
    let scopes = storage
        .states()?
        .into_iter()
        .map(|(scope, value)| {
            let checkpoint: Checkpoint = serde_json::from_value(value)?;
            Ok(ScopeStatus {
                domains: scope.split(',').map(str::to_owned).collect(),
                initialized: checkpoint.watermark.is_some(),
                watermark: checkpoint.watermark,
                active: checkpoint.run.map(|run| RunStatus {
                    id: run.id,
                    mode: run.mode,
                    started_at: run.started_at,
                    progress: run.progress,
                    report: run.report,
                }),
                last_run: checkpoint.last_run,
            })
        })
        .collect::<Result<_>>()?;
    Ok(Status {
        source: storage.source().to_owned(),
        revision: storage.revision()?,
        scopes,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Loading,
    Discovering,
    Fetching,
    Preparing,
    Committing,
    Committed,
    Complete,
}

impl std::fmt::Display for Phase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Loading => "loading checkpoint",
            Self::Discovering => "discovering",
            Self::Fetching => "fetching",
            Self::Preparing => "preparing batch",
            Self::Committing => "committing",
            Self::Committed => "committed",
            Self::Complete => "complete",
        })
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    #[serde(default)]
    watermark: Option<i64>,
    #[serde(default)]
    provider: Value,
    #[serde(default)]
    run: Option<ActiveRun>,
    #[serde(default)]
    last_run: Option<RunSummary>,
}

#[derive(Serialize)]
struct SavedCheckpoint<'a> {
    watermark: Option<i64>,
    provider: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<&'a ActiveRun>,
    last_run: &'a Option<RunSummary>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ActiveRun {
    id: String,
    mode: Mode,
    started_at: i64,
    provider: Value,
    report: Report,
    #[serde(default)]
    domains: BTreeSet<String>,
    progress: RunProgress,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunProgress {
    Discover { cursor: Value },
    Absence { after: String },
}

pub fn sync(
    provider: &mut (impl Source + ?Sized),
    storage: &mut Postgres,
    options: SyncOptions,
    now: i64,
    mut progress: impl FnMut(Phase, &Report),
) -> Result<RunSummary> {
    ensure!(
        (1..=1000).contains(&options.batch_size),
        "Batch size must be between 1 and 1000"
    );
    ensure!(now >= 0, "Invalid acquisition timestamp");
    ensure!(
        provider.fetch_batch_size() > 0,
        "Source fetch batch size must be positive"
    );
    ensure!(
        provider.source() == storage.source(),
        "Source and storage source do not match"
    );
    storage.begin()?;
    let mut report = Report::default();
    progress(Phase::Loading, &report);
    let mut checkpoint: Checkpoint = serde_json::from_value(storage.state()?)?;
    let mode = select_mode(
        checkpoint.watermark,
        checkpoint.run.as_ref().map(|run| run.mode),
        options,
    )?;
    if options.restart
        || checkpoint.run.as_ref().is_some_and(|run| {
            provider.resume(mode) == Resume::Restart
                && matches!(run.progress, RunProgress::Discover { .. })
        })
    {
        checkpoint.run = None;
    }
    let mut active = checkpoint.run.take().unwrap_or_else(|| ActiveRun {
        id: format!(
            "{}-{}-{}",
            provider.source(),
            mode.name(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ),
        mode,
        started_at: now,
        provider: checkpoint.provider.clone(),
        report: Report::default(),
        domains: BTreeSet::new(),
        progress: RunProgress::Discover {
            cursor: Value::Null,
        },
    });
    ensure!(
        !active.id.is_empty() && active.started_at >= 0 && active.started_at <= now,
        "Invalid active run"
    );
    let started = active.started_at;
    report = active.report.clone();
    let source = provider.source().to_owned();
    let observation = Observation {
        now,
        started,
        run_id: &active.id,
        mode,
        source: &source,
    };
    let watermark = checkpoint.watermark;
    if let Some(watermark) = watermark {
        ensure!(
            watermark >= 0 && watermark <= started,
            "Invalid synchronization watermark"
        );
    }
    provider.restore(&active.provider)?;
    persist(storage, &checkpoint, Some(&active), &Batch::default(), &[])?;
    loop {
        progress(Phase::Discovering, &report);
        let sweeping = matches!(active.progress, RunProgress::Absence { .. });
        let (mut changes, next) = match &active.progress {
            RunProgress::Absence { after } => {
                let keys = storage.unseen(&active.id, after)?;
                if keys.is_empty() {
                    break;
                }
                (
                    keys.into_iter()
                        .map(|key| provider.absence(key))
                        .collect::<Vec<_>>(),
                    None,
                )
            }
            RunProgress::Discover { cursor } => {
                let page = provider.discover(mode, watermark, cursor, started)?;
                ensure!(
                    page.next.as_ref() != Some(cursor),
                    "Source pagination did not advance"
                );
                let discovered: BTreeSet<_> = page
                    .changes
                    .iter()
                    .map(|change| change.key().to_owned())
                    .collect();
                let mut changes = page.changes;
                for membership in provider.memberships() {
                    let mut after = String::new();
                    loop {
                        let previous = storage.children(&membership.parent, &after)?;
                        if previous.is_empty() {
                            break;
                        }
                        for key in previous {
                            after = key.clone();
                            if !membership.members.contains(&key) && !discovered.contains(&key) {
                                changes.push(Change::Deleted { key, metadata: json!({"reason":"parent_membership_absent","parent":membership.parent}) });
                            }
                        }
                    }
                }
                (changes, page.next)
            }
        };
        changes = changes
            .into_iter()
            .map(|change| (change.key().to_owned(), change))
            .collect::<BTreeMap<_, _>>()
            .into_values()
            .collect();
        for chunk in changes.chunks(options.batch_size) {
            let keys: Vec<_> = chunk.iter().map(|change| change.key().to_owned()).collect();
            let entries = storage.entries(&keys)?;
            report.discovered += keys
                .iter()
                .filter(|key| {
                    !entries
                        .get(*key)
                        .is_some_and(|entry| entry.seen_run.as_deref() == Some(active.id.as_str()))
                })
                .count() as u64;
            let descriptors: BTreeMap<_, _> = keys
                .iter()
                .map(|key| {
                    let descriptor = descriptor(provider.descriptor(key).unwrap_or_else(|| {
                        entries
                            .get(key)
                            .map(|entry| entry.metadata.clone())
                            .unwrap_or_else(|| json!({}))
                    }));
                    (key.clone(), descriptor)
                })
                .collect();
            progress(Phase::Fetching, &report);
            let observed = |key: &String| {
                mode == Mode::Full
                    && !sweeping
                    && entries.get(key).is_some_and(|entry| {
                        !entry.deleted
                            && entry.seen_run.as_deref() == Some(active.id.as_str())
                            && descriptor(entry.metadata.clone()) == descriptors[key]
                    })
            };
            let fetch: Vec<_> = chunk
                .iter()
                .filter_map(|change| match change {
                    Change::Dirty(key) if !observed(key) => Some(FetchRequest {
                        key: key.clone(),
                        descriptor: descriptors[key].clone(),
                    }),
                    _ => None,
                })
                .collect();
            let mut fetched = BTreeMap::new();
            for requests in fetch.chunks(provider.fetch_batch_size().min(1000)) {
                let records = provider.fetch(requests)?;
                ensure!(
                    records.len() == requests.len(),
                    "Source returned incomplete batch"
                );
                report.fetched += requests.len() as u64;
                fetched.extend(
                    requests
                        .iter()
                        .zip(records)
                        .map(|(request, record)| (request.key.clone(), record)),
                );
            }
            progress(Phase::Preparing, &report);
            let mut batch = Batch::default();
            for change in chunk {
                let key = change.key();
                let (record, deletion) = match change {
                    Change::Dirty(key) => {
                        if observed(key) {
                            continue;
                        }
                        (
                            fetched.remove(key).context("Missing acquired record")?,
                            json!({"reason":"not_found"}),
                        )
                    }
                    Change::Deleted { metadata, .. } => (None, metadata.clone()),
                };
                let content = Content::new(record.as_ref().unwrap_or(&deletion), record.is_none())?;
                let entry = entries.get(key);
                let changed = !entry.is_some_and(|entry| entry.matches(&content));
                let metadata = observation.metadata(key, &descriptors[key], entry, changed)?;
                if changed || entry.map(|entry| &entry.metadata) != Some(&metadata) {
                    batch.updates.insert(
                        key.to_owned(),
                        Update {
                            content: changed.then_some(content),
                            metadata,
                        },
                    );
                }
            }
            if !sweeping {
                for change in chunk {
                    if let Change::Dirty(key) = change
                        && let Some((domain, _)) = key.split_once(':')
                    {
                        active.domains.insert(domain.into());
                    }
                }
            }
            if sweeping {
                active.progress = RunProgress::Absence {
                    after: keys.last().unwrap().clone(),
                };
            }
            report.changed += batch.content_changes() as u64;
            active.report = report.clone();
            progress(Phase::Committing, &report);
            persist(storage, &checkpoint, Some(&active), &batch, &keys)?;
            progress(Phase::Committed, &report);
        }
        if sweeping {
            continue;
        }
        active.provider = provider.checkpoint();
        if let Some(next) = next {
            active.progress = RunProgress::Discover { cursor: next };
            persist(storage, &checkpoint, Some(&active), &Batch::default(), &[])?;
        } else if mode == Mode::Full {
            ensure!(
                !active.domains.is_empty(),
                "Refusing to reconcile an empty catalogue"
            );
            for domain in provider.required_catalogue_domains() {
                ensure!(
                    active.domains.contains(domain),
                    "Refusing to reconcile an empty {domain} catalogue"
                );
            }
            active.progress = RunProgress::Absence {
                after: String::new(),
            };
            persist(storage, &checkpoint, Some(&active), &Batch::default(), &[])?;
        } else {
            break;
        }
    }
    report.complete = true;
    checkpoint.watermark = Some(started);
    checkpoint.provider = active.provider;
    let summary = RunSummary {
        id: active.id,
        mode,
        started_at: started,
        completed_at: chrono::Utc::now().timestamp(),
        report,
    };
    checkpoint.last_run = Some(summary.clone());
    persist(storage, &checkpoint, None, &Batch::default(), &[])?;
    progress(Phase::Complete, &summary.report);
    Ok(summary)
}

fn persist(
    storage: &mut Postgres,
    checkpoint: &Checkpoint,
    active: Option<&ActiveRun>,
    batch: &Batch,
    seen: &[String],
) -> Result<()> {
    storage.commit(Commit {
        state: &serde_json::to_value(SavedCheckpoint {
            watermark: checkpoint.watermark,
            provider: &checkpoint.provider,
            run: active,
            last_run: &checkpoint.last_run,
        })?,
        batch,
        seen,
        run_id: active.map(|run| run.id.as_str()).unwrap_or_default(),
    })
}

struct Observation<'a> {
    now: i64,
    started: i64,
    run_id: &'a str,
    mode: Mode,
    source: &'a str,
}

impl Observation<'_> {
    fn metadata(
        &self,
        key: &str,
        supplied: &Value,
        entry: Option<&Entry>,
        changed: bool,
    ) -> Result<Value> {
        let mut metadata = descriptor(supplied.clone());
        ensure!(metadata.is_object(), "Source metadata must be an object");
        if !changed
            && let Some(entry) = entry
            && descriptor(entry.metadata.clone()) == metadata
        {
            return Ok(entry.metadata.clone());
        }
        if let Some((domain, _)) = key.split_once(':') {
            metadata["domain"] = json!(domain);
        }
        metadata["source"] = json!(self.source);
        metadata["materialized_at"] = json!(self.now);
        metadata["run_started_at"] = json!(self.started);
        metadata["run_id"] = json!(self.run_id);
        metadata["mode"] = json!(self.mode.name());
        metadata["acquisition_version"] = json!(1);
        metadata["content_changed_at"] = if changed {
            json!(self.now)
        } else {
            entry
                .map(|entry| entry.metadata["content_changed_at"].clone())
                .unwrap_or(Value::Null)
        };
        Ok(metadata)
    }
}

fn descriptor(mut metadata: Value) -> Value {
    if let Some(fields) = metadata.as_object_mut() {
        for field in [
            "materialized_at",
            "run_started_at",
            "run_id",
            "operation",
            "mode",
            "source",
            "domain",
            "acquisition_version",
            "content_changed_at",
        ] {
            fields.remove(field);
        }
    }
    metadata
}

fn select_mode(
    watermark: Option<i64>,
    pending: Option<Mode>,
    options: SyncOptions,
) -> Result<Mode> {
    if !options.restart
        && let Some(mode) = pending
    {
        ensure!(
            !options.full || mode == Mode::Full,
            "An incremental sync is unfinished; resume without --full, or use --full --restart"
        );
        return Ok(mode);
    }
    Ok(if options.full || watermark.is_none() {
        Mode::Full
    } else {
        Mode::Incremental
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_initializes_resumes_and_restarts_without_changing_pending_modes_silently() {
        let options = SyncOptions::default();
        assert_eq!(select_mode(None, None, options).unwrap(), Mode::Full);
        assert_eq!(
            select_mode(Some(100), None, options).unwrap(),
            Mode::Incremental
        );
        assert_eq!(
            select_mode(Some(100), Some(Mode::Full), options).unwrap(),
            Mode::Full
        );
        let full = SyncOptions {
            full: true,
            ..options
        };
        assert_eq!(select_mode(Some(100), None, full).unwrap(), Mode::Full);
        assert!(select_mode(Some(100), Some(Mode::Incremental), full).is_err());
        assert_eq!(
            select_mode(
                Some(100),
                Some(Mode::Incremental),
                SyncOptions {
                    restart: true,
                    ..full
                }
            )
            .unwrap(),
            Mode::Full
        );
        assert_eq!(
            select_mode(
                Some(100),
                Some(Mode::Full),
                SyncOptions {
                    restart: true,
                    ..options
                }
            )
            .unwrap(),
            Mode::Incremental
        );
    }
}
