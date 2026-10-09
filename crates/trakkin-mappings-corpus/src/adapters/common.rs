use super::{Catalog, Entity};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) fn normalize_records(
    namespace: &str,
    input: &BTreeMap<String, Value>,
    normalize: impl Fn(&str) -> String,
) -> Result<BTreeMap<String, Value>> {
    let mut records = BTreeMap::new();
    for (key, payload) in input {
        let opaque = normalize_key(namespace, key, &normalize)?;
        ensure!(
            records.insert(opaque, payload.clone()).is_none(),
            "duplicate canonical native reference {key}"
        );
    }
    Ok(records)
}

pub(super) fn normalize_key(
    namespace: &str,
    key: &str,
    normalize: impl Fn(&str) -> String,
) -> Result<String> {
    let opaque = if let Some((source, opaque)) = key.split_once("://") {
        ensure!(source == namespace, "wrong-source native reference {key}");
        opaque.to_owned()
    } else {
        normalize(key)
    };
    ensure!(
        !opaque.is_empty()
            && !opaque.chars().any(|character| character.is_ascii_control()
                || character.is_ascii_whitespace()
                || ",<>[]".contains(character)),
        "invalid native reference {key}"
    );
    Ok(opaque)
}

pub(super) fn catalog(namespace: &str, records: &BTreeMap<String, Value>) -> Catalog {
    Catalog {
        entities: records
            .keys()
            .map(|opaque| (format!("{namespace}://{opaque}"), Entity::default()))
            .collect(),
    }
}

pub(super) fn number(value: &Value, field: &str) -> Result<u64> {
    value[field]
        .as_u64()
        .with_context(|| format!("native record lacks numeric {field}"))
}

pub(super) fn reference(namespace: &str, kind: &str, id: u64) -> String {
    format!("{namespace}://{kind}/{id}")
}

pub(super) fn check_id(opaque: &str, payload: &Value, kinds: &[&str]) -> Result<()> {
    let (kind, id) = opaque
        .split_once('/')
        .context("native reference needs a kind and ID")?;
    ensure!(
        kinds.contains(&kind),
        "unsupported native reference kind {kind}"
    );
    ensure!(
        !id.starts_with('0') && id.parse::<u64>().ok() == Some(number(payload, "id")?),
        "native reference and payload ID disagree: {opaque}"
    );
    Ok(())
}
