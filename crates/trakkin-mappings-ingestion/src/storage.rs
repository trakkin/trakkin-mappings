use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::{Content, Operation};

mod paimon;
mod routed;

pub use paimon::{Paimon, PaimonBridge};
pub use routed::RoutedStorage;

pub type Index = BTreeMap<String, Entry>;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Entry {
    pub hash: String,
    pub deleted: bool,
    pub metadata: Value,
}

impl Entry {
    pub fn matches(&self, content: &Content) -> bool {
        self.hash == content.hash() && self.deleted == content.deleted()
    }
}

#[derive(Clone, Debug, Serialize)]
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

    pub fn apply(&self, index: &mut Index) -> Result<()> {
        self.validate()?;
        for (key, update) in &self.updates {
            ensure!(
                update.content.is_some() || index.contains_key(key),
                "Metadata update has no record"
            );
        }
        for (key, update) in &self.updates {
            if let Some(content) = &update.content {
                index.insert(
                    key.clone(),
                    Entry {
                        hash: content.hash().to_owned(),
                        deleted: content.deleted(),
                        metadata: update.metadata.clone(),
                    },
                );
            } else if let Some(entry) = index.get_mut(key) {
                entry.metadata = update.metadata.clone();
            }
        }
        Ok(())
    }
}

pub trait Storage {
    fn index(&mut self) -> Result<Index>;
    fn checkpoint(&mut self, operation: Operation) -> Result<Value>;
    fn commit(&mut self, operation: Operation, checkpoint: &Value, batch: &Batch) -> Result<()>;
}
