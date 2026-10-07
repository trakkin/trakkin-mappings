use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::storage::Entry;

#[derive(Clone, Debug)]
pub enum Change {
    Dirty(String),
    Deleted { key: String, metadata: Value },
}

impl Change {
    pub fn key(&self) -> &str {
        match self {
            Self::Dirty(key) | Self::Deleted { key, .. } => key,
        }
    }
}

#[derive(Debug)]
pub struct Page {
    pub changes: Vec<Change>,
    pub next: Option<Value>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    Bootstrap,
    Sync,
    Reconcile,
}

impl Operation {
    pub const ALL: [Self; 3] = [Self::Bootstrap, Self::Sync, Self::Reconcile];

    pub fn name(self) -> &'static str {
        match self {
            Self::Bootstrap => "bootstrap",
            Self::Sync => "sync",
            Self::Reconcile => "reconcile",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resume {
    Cursor,
    Restart,
}

pub trait Provider {
    fn source(&self) -> &'static str;
    fn resume(&self, operation: Operation) -> Resume {
        match operation {
            Operation::Bootstrap | Operation::Sync => Resume::Cursor,
            Operation::Reconcile => Resume::Restart,
        }
    }
    fn restore(
        &mut self,
        _operation: Operation,
        _index: &BTreeMap<String, Entry>,
        _checkpoint: &Value,
    ) -> Result<()> {
        Ok(())
    }
    fn checkpoint(&self) -> Value {
        Value::Null
    }
    fn metadata(&self, _key: &str) -> Value {
        json!({})
    }
    fn discover(
        &mut self,
        operation: Operation,
        watermark: Option<i64>,
        cursor: &Value,
        until: i64,
    ) -> Result<Page>;
    fn fetch(&mut self, key: &str) -> Result<Option<Value>>;
    fn fetch_batch_size(&self) -> usize {
        1000
    }
    fn fetch_many(&mut self, keys: &[String]) -> Result<BTreeMap<String, Option<Value>>> {
        keys.iter()
            .map(|key| Ok((key.clone(), self.fetch(key)?)))
            .collect()
    }
}
