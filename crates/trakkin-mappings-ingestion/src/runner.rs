use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::storage::{Batch, Entry, Storage, Update};
use crate::{Change, Content, Operation, Provider, Resume};

#[derive(Clone, Default, Debug, Deserialize, Serialize)]
pub struct Report {
    pub discovered: u64,
    pub fetched: u64,
    pub changed: u64,
    pub complete: bool,
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
            Self::Loading => "loading checkpoint and index",
            Self::Discovering => "discovering",
            Self::Fetching => "fetching",
            Self::Preparing => "preparing batch",
            Self::Committing => "committing",
            Self::Committed => "committed",
            Self::Complete => "complete",
        })
    }
}

struct Observation<'a> {
    at: i64,
    started: i64,
    run_id: &'a str,
    operation: &'a str,
}

impl Observation<'_> {
    fn metadata(
        &self,
        provider: &(impl Provider + ?Sized),
        key: &str,
        entry: Option<&Entry>,
        changed: bool,
    ) -> Result<Value> {
        let mut metadata = descriptor(provider.metadata(key));
        ensure!(metadata.is_object(), "Provider metadata must be an object");
        if let Some((domain, _)) = key.split_once(':') {
            metadata["domain"] = json!(domain);
        }
        metadata["source"] = json!(provider.source());
        if !changed
            && let Some(entry) = entry
            && entry.metadata["acquisition_version"] == 1
            && descriptor(entry.metadata.clone()) == metadata
        {
            return Ok(entry.metadata.clone());
        }
        metadata["materialized_at"] = json!(self.at);
        metadata["run_started_at"] = json!(self.started);
        metadata["run_id"] = json!(self.run_id);
        metadata["operation"] = json!(self.operation);
        metadata["acquisition_version"] = json!(1);
        metadata["content_changed_at"] = if changed {
            json!(self.at)
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
            "acquisition_version",
            "content_changed_at",
        ] {
            fields.remove(field);
        }
    }
    metadata
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    #[serde(default)]
    watermark: Option<i64>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    provider: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run: Option<ActiveRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_run: Option<RunSummary>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ActiveRun {
    id: String,
    started_at: i64,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    cursor: Value,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RunSummary {
    run_id: String,
    operation: Operation,
    started_at: i64,
    completed_at: i64,
    source: String,
    report: Report,
}

pub fn run(
    provider: &mut (impl Provider + ?Sized),
    storage: &mut impl Storage,
    operation: Operation,
    batch_size: usize,
    now: i64,
    mut progress: impl FnMut(Phase, &Report),
) -> Result<Report> {
    ensure!(
        (1..=1000).contains(&batch_size),
        "Batch size must be between 1 and 1000"
    );
    let fetch_batch_size = provider.fetch_batch_size();
    ensure!(
        fetch_batch_size > 0,
        "Provider fetch batch size must be positive"
    );
    let name = operation.name();
    let mut report = Report::default();
    progress(Phase::Loading, &report);
    let saved: Checkpoint = serde_json::from_value(storage.checkpoint(operation)?)?;
    let mut active = saved.run.unwrap_or_else(|| ActiveRun {
        id: format!(
            "{}-{name}-{}",
            provider.source(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ),
        started_at: now,
        cursor: Value::Null,
    });
    ensure!(
        !active.id.is_empty() && active.started_at >= 0,
        "Invalid active run"
    );
    let resume = provider.resume(operation);
    if resume == Resume::Restart {
        active.cursor = Value::Null;
    }
    let started = active.started_at;
    let run_id = active.id.clone();
    let mut index = storage.index()?;
    provider.restore(operation, &index, &saved.provider)?;
    let watermark = saved.watermark;
    if operation == Operation::Sync {
        ensure!(
            watermark.is_some(),
            "Bootstrap the catalogue before incremental synchronization"
        );
        ensure!(
            watermark.is_some_and(|watermark| watermark >= 0 && watermark <= started),
            "Invalid synchronization watermark"
        );
    }
    let mut cursor = active.cursor.clone();
    let observation = Observation {
        at: now,
        started,
        run_id: &run_id,
        operation: name,
    };
    let mut checkpoint = Checkpoint {
        watermark,
        run: Some(active),
        provider: saved.provider,
        last_run: saved.last_run,
    };
    storage.commit(
        operation,
        &serde_json::to_value(&checkpoint)?,
        &Batch::default(),
    )?;
    loop {
        progress(Phase::Discovering, &report);
        let page = provider.discover(operation, watermark, &cursor, started)?;
        ensure!(
            page.next.as_ref() != Some(&cursor),
            "Provider pagination did not advance"
        );
        ensure!(
            page.changes
                .iter()
                .all(|change| !change.key().is_empty() && !change.key().starts_with('@')),
            "Provider returned invalid or reserved key"
        );
        report.discovered += page.changes.len() as u64;
        let changes: Vec<_> = page
            .changes
            .into_iter()
            .map(|change| (change.key().to_owned(), change))
            .collect::<BTreeMap<_, _>>()
            .into_values()
            .collect();
        let keys: Vec<_> = changes
            .iter()
            .filter_map(|change| match change {
                Change::Dirty(key)
                    if operation != Operation::Bootstrap
                        || !index.get(key).is_some_and(|entry| !entry.deleted) =>
                {
                    Some(key.clone())
                }
                _ => None,
            })
            .collect();
        let mut acquisitions = keys.chunks(fetch_batch_size);
        let mut fetched = BTreeMap::new();
        let chunks = changes.len().div_ceil(batch_size);
        for (chunk_number, chunk) in changes.chunks(batch_size).enumerate() {
            progress(Phase::Fetching, &report);
            for key in chunk.iter().filter_map(|change| match change {
                Change::Dirty(key)
                    if operation != Operation::Bootstrap
                        || !index.get(key).is_some_and(|entry| !entry.deleted) =>
                {
                    Some(key)
                }
                _ => None,
            }) {
                if fetched.contains_key(key) {
                    continue;
                }
                let keys = acquisitions.next().context("Missing acquisition batch")?;
                let records = provider.fetch_many(keys)?;
                ensure!(
                    records.len() == keys.len() && keys.iter().all(|key| records.contains_key(key)),
                    "Provider returned incomplete batch"
                );
                report.fetched += keys.len() as u64;
                fetched.extend(records);
            }
            let mut batch = Batch::default();
            progress(Phase::Preparing, &report);
            for change in chunk {
                let key = change.key();
                let (record, deletion) = match change {
                    Change::Dirty(key) => {
                        if operation == Operation::Bootstrap
                            && index.get(key).is_some_and(|entry| !entry.deleted)
                        {
                            continue;
                        }
                        (fetched.remove(key).unwrap(), json!({"reason":"not_found"}))
                    }
                    Change::Deleted { metadata, .. } => (None, metadata.clone()),
                };
                let content = Content::new(record.as_ref().unwrap_or(&deletion), record.is_none())?;
                let changed = !index.get(key).is_some_and(|entry| entry.matches(&content));
                let metadata = observation.metadata(provider, key, index.get(key), changed)?;
                if changed || index.get(key).map(|entry| &entry.metadata) != Some(&metadata) {
                    batch.updates.insert(
                        key.to_owned(),
                        Update {
                            content: changed.then_some(content),
                            metadata,
                        },
                    );
                }
            }
            let final_batch = chunk_number + 1 == chunks;
            if resume == Resume::Cursor
                && final_batch
                && let Some(next) = &page.next
            {
                checkpoint.run.as_mut().unwrap().cursor = next.clone();
            }
            if batch.updates.is_empty()
                && !(resume == Resume::Cursor && final_batch && page.next.is_some())
            {
                continue;
            }
            progress(Phase::Committing, &report);
            storage.commit(operation, &serde_json::to_value(&checkpoint)?, &batch)?;
            batch.apply(&mut index)?;
            report.changed += batch.content_changes() as u64;
            progress(Phase::Committed, &report);
        }
        if let Some(next) = page.next {
            cursor = next;
            if resume == Resume::Cursor {
                checkpoint.run.as_mut().unwrap().cursor = cursor.clone();
            }
            if resume == Resume::Cursor && changes.is_empty() {
                storage.commit(
                    operation,
                    &serde_json::to_value(&checkpoint)?,
                    &Batch::default(),
                )?;
            }
        } else {
            break;
        }
    }
    let completed = serde_json::to_value(Checkpoint {
        watermark: Some(started),
        provider: provider.checkpoint(),
        last_run: Some(RunSummary {
            run_id,
            operation,
            started_at: started,
            completed_at: chrono::Utc::now().timestamp(),
            source: provider.source().into(),
            report: Report {
                complete: true,
                ..report.clone()
            },
        }),
        ..Checkpoint::default()
    })?;
    storage.commit(operation, &completed, &Batch::default())?;
    if operation == Operation::Bootstrap {
        let sync: Checkpoint = serde_json::from_value(storage.checkpoint(Operation::Sync)?)?;
        if sync.watermark.is_none() {
            storage.commit(Operation::Sync, &completed, &Batch::default())?;
        }
    }
    report.complete = true;
    progress(Phase::Complete, &report);
    Ok(report)
}
