use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize)]
pub struct Content {
    payload: String,
    hash: String,
    deleted: bool,
}

impl Content {
    pub fn new(value: &Value, deleted: bool) -> Result<Self> {
        let payload = canonical_json(value)?;
        Ok(Self {
            hash: payload_hash(&payload),
            payload,
            deleted,
        })
    }

    pub fn payload(&self) -> &str {
        &self.payload
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }

    pub fn deleted(&self) -> bool {
        self.deleted
    }
}

pub fn canonical_json(record: &Value) -> Result<String> {
    let mut record = record.clone();
    record.sort_all_objects();
    Ok(serde_json::to_string(&record)?)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hashes_ignore_object_order_but_preserve_array_order() {
        assert_eq!(
            content_hash(&json!({"b":{"y":2,"x":1},"a":0})).unwrap(),
            content_hash(&json!({"a":0,"b":{"x":1,"y":2}})).unwrap()
        );
        assert_ne!(
            content_hash(&json!([1, 2])).unwrap(),
            content_hash(&json!([2, 1])).unwrap()
        );
    }

    #[test]
    fn prepared_content_hashes_the_exact_serialized_payload() {
        let value = json!({"score":1e-8,"id":1,"nested":{"b":2,"a":1}});
        let content = Content::new(&value, false).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(content.payload()).unwrap(),
            value
        );
        assert_eq!(content.hash(), payload_hash(content.payload()));
        assert_eq!(content.hash(), content_hash(&value).unwrap());
        assert!(!content.deleted());
        assert!(Content::new(&value, true).unwrap().deleted());
    }
}
