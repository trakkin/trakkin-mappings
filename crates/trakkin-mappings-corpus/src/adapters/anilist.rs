use super::{Catalog, SourceAdapter, Unit, common};
use anyhow::{Result, ensure};
use serde_json::Value;
use std::collections::BTreeMap;

pub struct AniList;

impl SourceAdapter for AniList {
    fn prepare(&self, namespace: &str, input: &BTreeMap<String, Value>) -> Result<Catalog> {
        ensure!(
            namespace == "co.anilist",
            "adapter does not support namespace {namespace}"
        );
        let records = common::normalize_records(namespace, input, |key| {
            if key.bytes().all(|byte| byte.is_ascii_digit()) {
                format!("media/{key}")
            } else {
                key.replacen(':', "/", 1)
            }
        })?;
        let mut catalog = common::catalog(namespace, &records);
        for (opaque, payload) in &records {
            common::check_id(opaque, payload, &["media"])?;
            let entity = catalog
                .entities
                .get_mut(&format!("{namespace}://{opaque}"))
                .unwrap();
            let dimensions: &[(&str, &str)] = match payload["type"].as_str() {
                Some("ANIME") => &[("episode", "episodes")],
                Some("MANGA") => &[("chapter", "chapters"), ("volume", "volumes")],
                _ => continue,
            };
            entity.hierarchy_complete =
                dimensions.len() == 1 && payload[dimensions[0].1].as_u64().is_some();
            for (dimension, field) in dimensions {
                entity.levels.push((*dimension).into());
                if let Some(count) = payload[field].as_u64() {
                    ensure!(
                        count <= 100_000 && entity.units.len() + count as usize <= 100_000,
                        "native scope exceeds 100,000 units"
                    );
                    for ordinal in 1..=count {
                        entity.units.push(Unit {
                            reference: format!("{namespace}://{opaque}/{dimension}/{ordinal}"),
                            level: (*dimension).into(),
                            attributes: BTreeMap::from([(
                                (*dimension).into(),
                                ordinal.to_string(),
                            )]),
                            coordinate: ordinal.to_string(),
                        });
                    }
                }
            }
        }
        Ok(catalog.finish())
    }
}
