use crate::Content;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

mod postgres;
pub use postgres::Postgres;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Entry {
    pub hash: String,
    pub deleted: bool,
    pub metadata: Value,
    pub seen_run: Option<String>,
}

impl Entry {
    pub fn matches(&self, content: &Content) -> bool {
        self.hash == content.hash() && self.deleted == content.deleted()
    }
}

#[derive(Clone, Debug)]
pub struct Update {
    pub content: Option<Content>,
    pub metadata: Value,
}

impl Update {
    pub fn record(value: &Value, metadata: Value) -> Result<Self> {
        Ok(Self {
            content: Some(Content::new(value, false)?),
            metadata,
        })
    }
    pub fn deletion(value: &Value, metadata: Value) -> Result<Self> {
        Ok(Self {
            content: Some(Content::new(value, true)?),
            metadata,
        })
    }
}

#[derive(Debug, Default)]
pub struct Batch {
    pub updates: BTreeMap<String, Update>,
}

impl Batch {
    pub fn content_changes(&self) -> usize {
        self.updates
            .values()
            .filter(|update| update.content.is_some())
            .count()
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.updates.len() <= 1000, "Commit exceeds 1000 records");
        for (key, update) in &self.updates {
            ensure!(
                !key.is_empty() && !key.starts_with('@'),
                "Invalid record key"
            );
            ensure!(
                update.metadata.is_object(),
                "Record metadata must be an object"
            );
        }
        Ok(())
    }
}

pub struct Commit<'a> {
    pub state: &'a Value,
    pub batch: &'a Batch,
    pub seen: &'a [String],
    pub run_id: &'a str,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Record {
    pub key: String,
    pub revision: i64,
    pub hash: String,
    pub deleted: bool,
    pub payload: Value,
    pub metadata: Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cursor {
    pub revision: i64,
    pub key: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Changes {
    pub records: Vec<Record>,
    pub cursor: Cursor,
    pub through: i64,
    pub complete: bool,
}
