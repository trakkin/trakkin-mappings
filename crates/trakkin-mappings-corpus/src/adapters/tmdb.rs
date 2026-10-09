use super::{
    Catalog, Entity, Episode, EpisodeOrder, Observation, SourceAdapter, Unit,
    common::{self, check_id, number, reference},
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::BTreeMap;

pub struct Tmdb;

impl Tmdb {
    pub fn default_order(parent: &str, records: &BTreeMap<String, Value>) -> Result<EpisodeOrder> {
        let indexed = records
            .iter()
            .map(|(key, payload)| Ok((Self::record_reference(key, payload)?.1, (key, payload))))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let (key, payload) = indexed.get(parent).context("TMDB series is absent")?;
        let mut order = EpisodeOrder {
            reference: parent.into(),
            context: BTreeMap::new(),
            observations: vec![(
                (*key).clone(),
                Observation {
                    field: "/seasons".into(),
                    value: payload["seasons"].clone(),
                },
            )],
            episodes: Vec::new(),
        };
        for (season_position, season) in payload["seasons"]
            .as_array()
            .context("TMDB default hierarchy has no season listing")?
            .iter()
            .enumerate()
        {
            let season_number = number(season, "season_number")?;
            let season_reference = reference("org.themoviedb", "season", number(season, "id")?);
            let (record, body, prefix) = match indexed.get(&season_reference) {
                Some((record, body)) => (*record, *body, String::new()),
                None => (*key, season, format!("/seasons/{season_position}")),
            };
            for (position, episode) in body["episodes"]
                .as_array()
                .context("TMDB default hierarchy is missing a season's episodes")?
                .iter()
                .enumerate()
            {
                ensure!(
                    number(episode, "season_number")? == season_number,
                    "TMDB episode belongs to another season"
                );
                let episode_reference =
                    reference("org.themoviedb", "episode", number(episode, "id")?);
                let path = format!("{prefix}/episodes/{position}");
                let mut observations: Vec<_> =
                    ["id", "season_number", "episode_number", "name", "air_date"]
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
                let external_id = indexed.get(&episode_reference).and_then(|(record, body)| {
                    body.pointer("/external_ids/tvdb_id")
                        .filter(|value| value.as_u64().is_some_and(|id| id > 0))
                        .map(|value| {
                            (
                                (*record).clone(),
                                Observation {
                                    field: "/external_ids/tvdb_id".into(),
                                    value: value.clone(),
                                },
                            )
                        })
                });
                let linked_reference = external_id.as_ref().map(|(_, observation)| {
                    format!(
                        "com.thetvdb://episode/{}",
                        observation.value.as_u64().unwrap()
                    )
                });
                observations.extend(external_id);
                order.episodes.push(Episode {
                    reference: episode_reference,
                    season: season_number,
                    episode: number(episode, "episode_number")?,
                    title: episode["name"].as_str().map(str::to_owned),
                    air_date: episode["air_date"].as_str().map(str::to_owned),
                    linked_reference,
                    observations,
                });
            }
        }
        Ok(order)
    }

    pub fn record_reference(key: &str, payload: &Value) -> Result<(String, String)> {
        let opaque = common::normalize_key("org.themoviedb", key, |key| key.replacen(':', "/", 1))?;
        let kind = opaque
            .split_once('/')
            .context("native reference needs a kind and ID")?
            .0;
        if kind == "episode_group" {
            ensure!(
                payload["id"].as_str() == opaque.strip_prefix("episode_group/"),
                "episode-group reference and ID disagree"
            );
        } else {
            check_id(&opaque, payload, &["movie", "tv", "season", "episode"])?;
        }
        Ok((kind.to_owned(), format!("org.themoviedb://{opaque}")))
    }
}

impl SourceAdapter for Tmdb {
    fn prepare(&self, namespace: &str, input: &BTreeMap<String, Value>) -> Result<Catalog> {
        ensure!(
            namespace == "org.themoviedb",
            "adapter does not support namespace {namespace}"
        );
        let records = common::normalize_records(namespace, input, |key| key.replacen(':', "/", 1))?;
        let mut catalog = common::catalog(namespace, &records);
        for (opaque, payload) in &records {
            if opaque.starts_with("episode_group/") {
                ensure!(
                    payload["id"].as_str() == opaque.strip_prefix("episode_group/"),
                    "episode-group reference and ID disagree"
                );
                continue;
            }
            check_id(opaque, payload, &["movie", "tv", "season", "episode"])?;
            if opaque.starts_with("tv/") {
                let mut entity = Entity {
                    levels: vec!["season".into(), "episode".into()],
                    hierarchy_complete: payload["seasons"].is_array(),
                    contexts: BTreeMap::from([("episode_group".into(), String::new())]),
                    ..Entity::default()
                };
                for season in payload["seasons"].as_array().into_iter().flatten() {
                    let season_id = number(season, "id")?;
                    let season_number = number(season, "season_number")?;
                    entity.units.push(Unit {
                        reference: reference(namespace, "season", season_id),
                        level: "season".into(),
                        attributes: BTreeMap::from([("season".into(), season_number.to_string())]),
                        coordinate: season_number.to_string(),
                    });
                    let body = records
                        .get(&format!("season/{season_id}"))
                        .unwrap_or(season);
                    if !body["episodes"].is_array() {
                        entity.hierarchy_complete = false;
                    }
                    for episode in body["episodes"].as_array().into_iter().flatten() {
                        let episode_number = number(episode, "episode_number")?;
                        ensure!(
                            number(episode, "season_number")? == season_number,
                            "TMDB episode belongs to another season"
                        );
                        entity.units.push(Unit {
                            reference: reference(namespace, "episode", number(episode, "id")?),
                            level: "episode".into(),
                            attributes: BTreeMap::from([
                                ("season".into(), season_number.to_string()),
                                ("episode".into(), episode_number.to_string()),
                            ]),
                            coordinate: format!("{season_number}/{episode_number}"),
                        });
                    }
                }
                for group in payload["episode_groups"]["results"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    let id = group["id"].as_str().context("episode group lacks ID")?;
                    if let Some(body) = records.get(&format!("episode_group/{id}")) {
                        add_group(namespace, id, body, &mut entity)?;
                    }
                }
                catalog
                    .entities
                    .insert(format!("{namespace}://{opaque}"), entity);
            } else if opaque.starts_with("season/") {
                let mut entity = Entity {
                    levels: vec!["episode".into()],
                    hierarchy_complete: payload["episodes"].is_array(),
                    ..Entity::default()
                };
                for episode in payload["episodes"].as_array().into_iter().flatten() {
                    let ordinal = number(episode, "episode_number")?;
                    entity.units.push(Unit {
                        reference: reference(namespace, "episode", number(episode, "id")?),
                        level: "episode".into(),
                        attributes: BTreeMap::from([("episode".into(), ordinal.to_string())]),
                        coordinate: ordinal.to_string(),
                    });
                }
                catalog
                    .entities
                    .insert(format!("{namespace}://{opaque}"), entity);
            }
        }
        Ok(catalog.finish())
    }
}

fn add_group(namespace: &str, id: &str, body: &Value, entity: &mut Entity) -> Result<()> {
    if !entity.levels.contains(&"group".to_owned()) {
        entity.levels.insert(1, "group".into());
    }
    for group in body["groups"]
        .as_array()
        .context("episode group lacks groups")?
    {
        let group_number = number(group, "order")?
            .checked_add(1)
            .context("group order exceeds supported integer range")?;
        for episode in group["episodes"]
            .as_array()
            .context("group lacks episodes")?
        {
            let ordinal = number(episode, "order")?
                .checked_add(1)
                .context("episode order exceeds supported integer range")?;
            entity.units.push(Unit {
                reference: reference(namespace, "episode", number(episode, "id")?),
                level: "episode".into(),
                attributes: BTreeMap::from([
                    ("episode_group".into(), id.into()),
                    ("group".into(), group_number.to_string()),
                    ("episode".into(), ordinal.to_string()),
                ]),
                coordinate: format!("{group_number}/{ordinal}"),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_hierarchy_does_not_require_alternate_groups_but_requires_all_seasons() {
        let mut records = BTreeMap::from([
            (
                "tv:7".into(),
                json!({"id":7,"seasons":[{"id":70,"season_number":1}],"episode_groups":{"results":[{"id":"unavailable"}]}}),
            ),
            (
                "season:70".into(),
                json!({"id":70,"episodes":[{"id":701,"season_number":1,"episode_number":1}]}),
            ),
        ]);
        let order = Tmdb::default_order("org.themoviedb://tv/7", &records).unwrap();
        assert!(order.context.is_empty());
        assert_eq!(order.episodes[0].reference, "org.themoviedb://episode/701");
        records.remove("season:70");
        assert!(Tmdb::default_order("org.themoviedb://tv/7", &records).is_err());
    }
}
