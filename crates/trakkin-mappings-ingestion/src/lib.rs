pub mod dataset;
pub mod providers;
pub mod storage;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use storage::{Batch, Entry, Storage};

#[derive(Clone, Debug)]
pub enum Change {
    Dirty(String),
    Deleted {
        key: String,
        metadata: Value,
    },
    Membership {
        parent: String,
        keys: BTreeSet<String>,
    },
}

#[derive(Debug)]
pub struct Page {
    pub changes: Vec<Change>,
    pub next: Option<Value>,
}

pub trait Provider {
    fn source(&self) -> Option<&'static str> {
        None
    }
    fn restore(&mut self, _index: &BTreeMap<String, Entry>) -> Result<()> {
        Ok(())
    }
    fn metadata(&self, _key: &str) -> Value {
        json!({})
    }
    fn confirm_absence(&self, _key: &str) -> bool {
        true
    }
    fn select_domain(&mut self, _domain: &str) {}
    fn select_domains(&mut self, domains: &[&str]) {
        if let [domain] = domains {
            self.select_domain(domain);
        }
    }
    fn enumerate(&mut self, cursor: &Value, started: i64) -> Result<Page>;
    fn discover_changes(&mut self, watermark: i64, cursor: &Value, until: i64) -> Result<Page>;
    fn fetch(&mut self, key: &str) -> Result<Option<Value>>;
    fn fetch_many(&mut self, keys: &[String]) -> Result<BTreeMap<String, Option<Value>>> {
        keys.iter()
            .map(|key| Ok((key.clone(), self.fetch(key)?)))
            .collect()
    }
    fn restart_changes(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Bootstrap,
    Sync,
    Reconcile,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Bootstrap => "bootstrap",
            Self::Sync => "sync",
            Self::Reconcile => "reconcile",
        }
    }
}

#[derive(Default, Debug, Serialize)]
pub struct Report {
    pub discovered: u64,
    pub fetched: u64,
    pub changed: u64,
    pub complete: bool,
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
    ) -> Value {
        let mut metadata = provider.metadata(key);
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
        metadata = descriptor(metadata);
        if let Some((domain, _)) = key.split_once(':') {
            metadata["domain"] = json!(domain);
        }
        if let Some(source) = provider.source() {
            metadata["source"] = json!(source);
        }
        if !changed
            && let Some(entry) = entry
            && entry.metadata["acquisition_version"] == 1
            && descriptor(entry.metadata.clone()) == metadata
        {
            return entry.metadata.clone();
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
        metadata
    }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    #[serde(default)]
    pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    started: Option<i64>,
    #[serde(default)]
    watermark: Option<i64>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    cursor: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_run: Option<Value>,
}

pub fn canonical_json(record: &Value) -> Result<String> {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(object) => {
                let ordered: BTreeMap<_, _> = object
                    .iter()
                    .map(|(key, value)| (key.clone(), sorted(value)))
                    .collect();
                serde_json::to_value(ordered).expect("JSON object")
            }
            Value::Array(array) => Value::Array(array.iter().map(sorted).collect()),
            _ => value.clone(),
        }
    }
    Ok(serde_json::to_string(&sorted(record))?)
}

pub fn content_hash(record: &Value) -> Result<String> {
    Ok(payload_hash(&canonical_json(record)?))
}

pub(crate) fn payload_hash(payload: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    Sha256::digest(payload.as_bytes())
        .iter()
        .flat_map(|byte| {
            [
                HEX[(byte >> 4) as usize] as char,
                HEX[(byte & 15) as usize] as char,
            ]
        })
        .collect()
}

pub fn run(
    provider: &mut (impl Provider + ?Sized),
    storage: &mut impl Storage,
    operation: Operation,
    batch_size: usize,
    now: i64,
    mut progress: impl FnMut(&str, &Report),
) -> Result<Report> {
    ensure!(
        batch_size > 0 && batch_size <= 1000,
        "Batch size must be between 1 and 1000"
    );
    let name = operation.name();
    let mut report = Report::default();
    progress("loading checkpoint and index", &report);
    let saved: Checkpoint = serde_json::from_value(storage.checkpoint(name)?)?;
    let pending = saved.pending;
    ensure!(
        !pending || saved.started.is_some(),
        "Pending checkpoint is missing its start time"
    );
    let started = if operation == Operation::Bootstrap {
        saved.started.or(saved.watermark).unwrap_or(now)
    } else if pending {
        saved.started.unwrap()
    } else {
        now
    };
    let mut index = storage.index()?;
    provider.restore(&index)?;
    let mut parents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (key, entry) in &index {
        if !entry.deleted
            && let Some(parent) = entry.metadata["parent"].as_str()
        {
            parents
                .entry(parent.into())
                .or_default()
                .insert(key.clone());
        }
    }
    let run_id = if pending { saved.run_id.clone() } else { None }.unwrap_or_else(|| {
        format!(
            "{}-{name}-{}",
            provider.source().unwrap_or("fixture"),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        )
    });
    let watermark = saved.watermark;
    if operation == Operation::Sync {
        ensure!(
            watermark.is_some(),
            "Bootstrap the catalogue before incremental synchronization"
        );
    }
    let mut cursor = if pending && operation == Operation::Sync && !provider.restart_changes() {
        saved.cursor
    } else {
        Value::Null
    };
    let mut members = BTreeSet::new();
    let observation = Observation {
        at: now,
        started,
        run_id: &run_id,
        operation: name,
    };
    let mut checkpoint = Checkpoint {
        pending: true,
        started: Some(started),
        watermark,
        cursor: cursor.clone(),
        run_id: Some(run_id.clone()),
        last_run: saved.last_run,
    };
    storage.commit(name, &serde_json::to_value(&checkpoint)?, &Batch::default())?;
    loop {
        progress("discovering", &report);
        let mut page = if operation == Operation::Sync {
            provider.discover_changes(watermark.unwrap(), &cursor, started)?
        } else {
            provider.enumerate(&cursor, started)?
        };
        ensure!(
            page.next.as_ref() != Some(&cursor),
            "Provider pagination did not advance"
        );
        let mut removals = Vec::new();
        for change in &page.changes {
            if let Change::Membership { parent, keys } = change {
                for key in parents.get(parent).into_iter().flatten() {
                    if !keys.contains(key) {
                        removals.push(Change::Deleted {
                            key: key.clone(),
                            metadata: json!({"reason":"parent_membership_absent","parent":parent}),
                        });
                    }
                }
            }
        }
        page.changes
            .retain(|change| !matches!(change, Change::Membership { .. }));
        ensure!(
            page.changes.iter().all(|change| match change {
                Change::Dirty(key) | Change::Deleted { key, .. } => !key.starts_with('@'),
                Change::Membership { .. } => unreachable!(),
            }),
            "Provider returned reserved key"
        );
        let keys: Vec<_> = page
            .changes
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
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        ensure!(
            keys.iter().all(|key| !key.starts_with('@')),
            "Provider returned reserved key"
        );
        progress("fetching", &report);
        let fetched = acquire(provider, &keys)?;
        report.fetched += keys.len() as u64;
        page.changes.extend(removals.into_iter().filter(|change| {
            let Change::Deleted { key, metadata } = change else {
                unreachable!()
            };
            !(fetched.get(key).is_some_and(Option::is_some)
                && provider.metadata(key)["parent"]
                    .as_str()
                    .is_some_and(|parent| Some(parent) != metadata["parent"].as_str()))
        }));
        report.discovered += page.changes.len() as u64;
        let chunks = page.changes.len().div_ceil(batch_size);
        for (chunk_number, chunk) in page.changes.chunks(batch_size).enumerate() {
            let mut batch = Batch::default();
            progress("preparing batch", &report);
            for change in chunk {
                match change {
                    Change::Dirty(key) => {
                        ensure!(!key.starts_with('@'), "Provider returned reserved key");
                        members.insert(key.clone());
                        batch.records.remove(key);
                        batch.deleted.remove(key);
                        if operation == Operation::Bootstrap
                            && index.get(key).is_some_and(|entry| !entry.deleted)
                        {
                            continue;
                        }
                        let Some(record) = fetched.get(key).cloned() else {
                            continue;
                        };
                        match record {
                            Some(record) => {
                                let hash = content_hash(&record)?;
                                let changed = !index
                                    .get(key)
                                    .is_some_and(|entry| entry.hash == hash && !entry.deleted);
                                let metadata =
                                    observation.metadata(provider, key, index.get(key), changed);
                                if index.get(key).map(|entry| &entry.metadata) != Some(&metadata) {
                                    batch.metadata.insert(key.clone(), metadata);
                                }
                                if changed {
                                    batch.deleted.remove(key);
                                    batch.records.insert(key.clone(), record);
                                }
                            }
                            None => {
                                batch.records.remove(key);
                                let deletion = json!({"reason":"not_found"});
                                let hash = content_hash(&deletion)?;
                                let changed = !index
                                    .get(key)
                                    .is_some_and(|entry| entry.deleted && entry.hash == hash);
                                let metadata =
                                    observation.metadata(provider, key, index.get(key), changed);
                                if index.get(key).map(|entry| &entry.metadata) != Some(&metadata) {
                                    batch.metadata.insert(key.clone(), metadata);
                                }
                                if changed {
                                    batch.deleted.insert(key.clone(), deletion);
                                }
                            }
                        }
                    }
                    Change::Deleted { key, metadata } => {
                        batch.records.remove(key);
                        let hash = content_hash(metadata)?;
                        let changed = !index
                            .get(key)
                            .is_some_and(|entry| entry.deleted && entry.hash == hash);
                        if changed {
                            batch.deleted.insert(key.clone(), metadata.clone());
                        }
                        let metadata = observation.metadata(provider, key, index.get(key), changed);
                        if index.get(key).map(|entry| &entry.metadata) != Some(&metadata) {
                            batch.metadata.insert(key.clone(), metadata);
                        }
                    }
                    Change::Membership { .. } => unreachable!(),
                }
            }
            if batch.records.is_empty() && batch.deleted.is_empty() && batch.metadata.is_empty() {
                continue;
            }
            if chunk_number + 1 == chunks
                && let Some(next) = &page.next
            {
                checkpoint.cursor = next.clone();
            }
            progress("committing", &report);
            report.changed += storage.commit(name, &serde_json::to_value(&checkpoint)?, &batch)?;
            let touched: BTreeSet<_> = batch
                .metadata
                .keys()
                .chain(batch.records.keys())
                .chain(batch.deleted.keys())
                .cloned()
                .collect();
            for key in &touched {
                if let Some(parent) = index
                    .get(key)
                    .and_then(|entry| entry.metadata["parent"].as_str())
                    && let Some(keys) = parents.get_mut(parent)
                {
                    keys.remove(key);
                }
            }
            apply(&mut index, &batch)?;
            for key in touched {
                if let Some(entry) = index.get(&key)
                    && !entry.deleted
                    && let Some(parent) = entry.metadata["parent"].as_str()
                {
                    parents.entry(parent.into()).or_default().insert(key);
                }
            }
            progress("committed", &report);
        }
        if let Some(next) = page.next {
            cursor = next;
            checkpoint.cursor = cursor.clone();
        } else {
            break;
        }
    }
    if operation == Operation::Reconcile {
        ensure!(
            !members.is_empty(),
            "Refusing to delete from an empty catalogue; investigate upstream"
        );
        let absent: Vec<_> = index
            .iter()
            .filter(|(key, entry)| !entry.deleted && !members.contains(*key))
            .map(|(key, _)| key.clone())
            .collect();
        for chunk in absent.chunks(batch_size) {
            let mut batch = Batch::default();
            progress("confirming catalogue absences", &report);
            let confirmations: Vec<_> = chunk
                .iter()
                .filter(|key| provider.confirm_absence(key))
                .cloned()
                .collect();
            let mut fetched = acquire(provider, &confirmations)?;
            for key in chunk {
                fetched.entry(key.clone()).or_insert(None);
            }
            for key in chunk {
                if let Some(record) = fetched[key].clone() {
                    let hash = content_hash(&record)?;
                    let changed = !index
                        .get(key)
                        .is_some_and(|entry| !entry.deleted && entry.hash == hash);
                    let metadata = observation.metadata(provider, key, index.get(key), changed);
                    if index.get(key).map(|entry| &entry.metadata) != Some(&metadata) {
                        batch.metadata.insert(key.clone(), metadata);
                    }
                    if changed {
                        batch.records.insert(key.clone(), record);
                    }
                } else {
                    batch.metadata.insert(
                        key.clone(),
                        observation.metadata(provider, key, index.get(key), true),
                    );
                    batch
                        .deleted
                        .insert(key.clone(), json!({"reason":"catalogue_absent"}));
                }
                report.fetched += u64::from(provider.confirm_absence(key));
            }
            if batch.records.is_empty() && batch.deleted.is_empty() && batch.metadata.is_empty() {
                continue;
            }
            progress("committing", &report);
            report.changed += storage.commit(name, &serde_json::to_value(&checkpoint)?, &batch)?;
            progress("committed", &report);
        }
    }
    let completed = serde_json::to_value(Checkpoint {
        watermark: Some(started),
        run_id: Some(run_id.clone()),
        last_run: Some(
            json!({"run_id":run_id,"operation":name,"started_at":started,"completed_at":chrono::Utc::now().timestamp(),"source":provider.source(),"report":{"discovered":report.discovered,"fetched":report.fetched,"changed":report.changed,"complete":true}}),
        ),
        ..Checkpoint::default()
    })?;
    storage.commit(name, &completed, &Batch::default())?;
    if operation == Operation::Bootstrap {
        let sync: Checkpoint = serde_json::from_value(storage.checkpoint("sync")?)?;
        if sync.watermark.is_none() {
            storage.commit("sync", &completed, &Batch::default())?;
        }
    }
    report.complete = true;
    progress("complete", &report);
    Ok(report)
}

fn acquire(
    provider: &mut (impl Provider + ?Sized),
    keys: &[String],
) -> Result<BTreeMap<String, Option<Value>>> {
    let fetched = provider.fetch_many(keys)?;
    ensure!(
        fetched.len() == keys.len() && keys.iter().all(|key| fetched.contains_key(key)),
        "Provider returned incomplete batch"
    );
    Ok(fetched)
}

fn apply(index: &mut BTreeMap<String, Entry>, batch: &Batch) -> Result<()> {
    for (key, metadata) in &batch.metadata {
        if let Some(entry) = index.get_mut(key) {
            entry.metadata = metadata.clone();
        }
    }
    for (key, record) in &batch.records {
        index.insert(
            key.clone(),
            Entry {
                hash: content_hash(record)?,
                deleted: false,
                metadata: batch.metadata.get(key).cloned().unwrap_or(json!({})),
            },
        );
    }
    for (key, metadata) in &batch.deleted {
        index.insert(
            key.clone(),
            Entry {
                hash: content_hash(metadata)?,
                deleted: true,
                metadata: batch.metadata.get(key).cloned().unwrap_or(json!({})),
            },
        );
    }
    Ok(())
}
