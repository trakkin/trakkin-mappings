use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
pub enum Mode {
    Incremental,
    Full,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Incremental => "incremental",
            Self::Full => "full",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resume {
    Cursor,
    Restart,
}

pub struct Membership {
    pub parent: String,
    pub members: std::collections::BTreeSet<String>,
}

#[derive(Clone, Debug)]
pub struct FetchRequest {
    pub key: String,
    pub descriptor: Value,
}

pub trait Source {
    fn source(&self) -> &str;
    fn resume(&self, mode: Mode) -> Resume {
        match mode {
            Mode::Incremental => Resume::Cursor,
            Mode::Full => Resume::Restart,
        }
    }
    fn restore(&mut self, _checkpoint: &Value) -> Result<()> {
        Ok(())
    }
    fn memberships(&self) -> Vec<Membership> {
        Vec::new()
    }
    fn absence(&self, key: String) -> Change {
        Change::Dirty(key)
    }
    fn required_catalogue_domains(&self) -> Vec<&str> {
        Vec::new()
    }
    fn checkpoint(&self) -> Value {
        Value::Null
    }
    fn descriptor(&self, _key: &str) -> Option<Value> {
        None
    }
    fn discover(
        &mut self,
        mode: Mode,
        watermark: Option<i64>,
        cursor: &Value,
        until: i64,
    ) -> Result<Page>;
    fn fetch(&mut self, requests: &[FetchRequest]) -> Result<Vec<Option<Value>>>;
    fn fetch_batch_size(&self) -> usize {
        1000
    }
}
