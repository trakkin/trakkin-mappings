use super::{
    Catalog, Entity, Episode, EpisodeOrder, Observation, SourceAdapter, Unit,
    common::{self, check_id, number, reference},
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub struct Tvdb;

impl Tvdb {
    pub fn default_order(parent: &str, records: &BTreeMap<String, Value>) -> Result<EpisodeOrder> {
        let (record, payload) = records
            .iter()
            .find(|(key, _)| {
                common::normalize_key("com.thetvdb", key, |key| key.replacen(':', "/", 1))
                    .is_ok_and(|opaque| format!("com.thetvdb://{opaque}") == parent)
            })
            .context("TVDB series is absent")?;
        let body = payload.get("series").unwrap_or(payload);
        let opaque = parent
            .strip_prefix("com.thetvdb://")
            .context("invalid TVDB series reference")?;
        check_id(opaque, body, &["series"])?;
        let kind = default_type(body)?;
        let seasons = body["seasons"]
            .as_array()
            .context("TVDB default hierarchy has no season listing")?
            .iter()
            .map(|season| number(season, "number"))
            .collect::<Result<BTreeSet<_>>>()?;
        let prefix = if payload.get("series").is_some() {
            "/series"
        } else {
            ""
        };
        let mut order = EpisodeOrder {
            reference: parent.into(),
            context: BTreeMap::from([("order".into(), normalize_order(kind))]),
            observations: ["defaultSeasonType", "seasonTypes", "seasons"]
                .into_iter()
                .map(|field| {
                    (
                        record.clone(),
                        Observation {
                            field: format!("{prefix}/{field}"),
                            value: body[field].clone(),
                        },
                    )
                })
                .collect(),
            episodes: Vec::new(),
        };
        for (position, episode) in payload["episode_orders"][kind]
            .as_array()
            .context("TVDB default episode order is absent")?
            .iter()
            .enumerate()
        {
            let path = format!("/episode_orders/{kind}/{position}");
            let numbering = if kind == "absolute" {
                "absoluteNumber"
            } else {
                "number"
            };
            ensure!(
                seasons.contains(&number(episode, "seasonNumber")?),
                "TVDB default order contains an unlisted season"
            );
            let observations = ["id", "seasonNumber", numbering, "name", "aired"]
                .map(|field| {
                    (
                        record.clone(),
                        Observation {
                            field: format!("{path}/{field}"),
                            value: episode[field].clone(),
                        },
                    )
                })
                .into();
            order.episodes.push(Episode {
                reference: Self::episode_reference(number(episode, "id")?)?,
                season: number(episode, "seasonNumber")?,
                episode: number(episode, numbering)?,
                title: episode["name"].as_str().map(str::to_owned),
                air_date: episode["aired"].as_str().map(str::to_owned),
                linked_reference: None,
                observations,
            });
        }
        Ok(order)
    }

    pub fn episode_reference(id: u64) -> Result<String> {
        ensure!(id > 0, "TVDB episode ID must be positive");
        Ok(reference("com.thetvdb", "episode", id))
    }
}

impl SourceAdapter for Tvdb {
    fn prepare(&self, namespace: &str, input: &BTreeMap<String, Value>) -> Result<Catalog> {
        ensure!(
            namespace == "com.thetvdb",
            "adapter does not support namespace {namespace}"
        );
        let records = common::normalize_records(namespace, input, |key| key.replacen(':', "/", 1))?;
        let mut catalog = common::catalog(namespace, &records);
        for (opaque, payload) in &records {
            let body = payload.get("series").unwrap_or(payload);
            check_id(opaque, body, &["series", "movie", "season", "episode"])?;
            if !opaque.starts_with("series/") {
                continue;
            }
            let default_type = default_type(body)?;
            let default = normalize_order(default_type);
            let mut entity = Entity {
                levels: vec!["season".into(), "episode".into()],
                contexts: BTreeMap::from([("order".into(), default.clone())]),
                hierarchy_complete: payload["episode_orders"].get(default_type).is_some()
                    && body["seasons"].is_array(),
                ..Entity::default()
            };
            let mut known_seasons = BTreeSet::new();
            for season in body["seasons"].as_array().into_iter().flatten() {
                let ordinal = number(season, "number")?;
                known_seasons.insert(ordinal);
                entity.units.push(Unit {
                    reference: reference(namespace, "season", number(season, "id")?),
                    level: "season".into(),
                    attributes: BTreeMap::from([
                        ("season".into(), ordinal.to_string()),
                        ("order".into(), default.clone()),
                    ]),
                    coordinate: ordinal.to_string(),
                });
            }
            for (order, episodes) in payload["episode_orders"].as_object().into_iter().flatten() {
                for episode in episodes
                    .as_array()
                    .context("episode order is not an array")?
                {
                    let season = number(episode, "seasonNumber")?;
                    let ordinal = number(
                        episode,
                        if order == "absolute" {
                            "absoluteNumber"
                        } else {
                            "number"
                        },
                    )?;
                    if normalize_order(order) == default && !known_seasons.contains(&season) {
                        entity.hierarchy_complete = false;
                    }
                    entity.units.push(Unit {
                        reference: reference(namespace, "episode", number(episode, "id")?),
                        level: "episode".into(),
                        attributes: BTreeMap::from([
                            ("season".into(), season.to_string()),
                            ("episode".into(), ordinal.to_string()),
                            ("order".into(), normalize_order(order)),
                        ]),
                        coordinate: if order == "absolute" {
                            ordinal.to_string()
                        } else {
                            format!("{season}/{ordinal}")
                        },
                    });
                }
            }
            catalog
                .entities
                .insert(format!("{namespace}://{opaque}"), entity);
        }
        Ok(catalog.finish())
    }
}

fn normalize_order(order: &str) -> String {
    if order == "official" {
        "aired".to_owned()
    } else {
        order.to_owned()
    }
}

fn default_type(body: &Value) -> Result<&str> {
    let default = body["defaultSeasonType"]
        .as_u64()
        .context("TVDB series has no default season type")?;
    body["seasonTypes"]
        .as_array()
        .context("TVDB series has no season types")?
        .iter()
        .find(|kind| kind["id"].as_u64() == Some(default))
        .and_then(|kind| kind["type"].as_str())
        .filter(|kind| !kind.is_empty())
        .context("TVDB declared default season type is absent")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use trakkin_mappings_language::{Resolver, parse_record};

    #[test]
    fn default_order_rejects_a_mismatched_series_payload() {
        let records = BTreeMap::from([(
            "series:8".into(),
            json!({
                "series":{"id":9,"defaultSeasonType":1,"seasonTypes":[{"id":1,"type":"official"}],"seasons":[{"id":80,"number":1}]},
                "episode_orders":{"official":[{"id":801,"seasonNumber":1,"number":1}]}
            }),
        )]);
        assert!(Tvdb::default_order("com.thetvdb://series/8", &records).is_err());
    }

    #[test]
    fn declared_absolute_default_uses_absolute_numbers_and_never_falls_back() {
        let mut records = BTreeMap::from([(
            "series:8".into(),
            json!({
                "series":{"id":8,"defaultSeasonType":3,"seasonTypes":[{"id":1,"type":"official"},{"id":3,"type":"absolute"}],"seasons":[{"id":80,"number":2}]},
                "episode_orders":{
                    "official":[{"id":801,"seasonNumber":2,"number":1}],
                    "absolute":[{"id":801,"seasonNumber":2,"number":1,"absoluteNumber":42}]
                }
            }),
        )]);
        let order = Tvdb::default_order("com.thetvdb://series/8", &records).unwrap();
        assert_eq!(order.context["order"], "absolute");
        assert_eq!(order.episodes[0].episode, 42);
        let catalog = Tvdb.prepare("com.thetvdb", &records).unwrap();
        let record = parse_record(
            "com.thetvdb://series/8 :: order=absolute,season=2,episode=42 <=> test://episode/1",
        )
        .unwrap();
        assert_eq!(
            catalog
                .resolve(record.statement.left.selections()[0])
                .unwrap()
                .units,
            ["com.thetvdb://episode/801"]
        );
        records.get_mut("series:8").unwrap()["episode_orders"]
            .as_object_mut()
            .unwrap()
            .remove("absolute");
        assert!(Tvdb::default_order("com.thetvdb://series/8", &records).is_err());
        records.get_mut("series:8").unwrap()["series"]["defaultSeasonType"] = json!(99);
        assert!(Tvdb::default_order("com.thetvdb://series/8", &records).is_err());
    }
}
