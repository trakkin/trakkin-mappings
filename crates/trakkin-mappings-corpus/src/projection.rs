use anyhow::{Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use unicode_normalization::UnicodeNormalization;

pub(crate) struct Fact {
    pub uri: String,
    pub kind: String,
    pub year: Option<i32>,
    pub titles: Vec<(String, String)>,
    pub identifiers: Vec<(String, String)>,
}

pub(crate) fn reference(source: &str, key: &str) -> Result<String> {
    let (kind, id) = key
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid native key"))?;
    let reference = format!("{source}://{kind}/{id}");
    trakkin_mappings_language::parse_reference(&reference)?;
    Ok(reference)
}

pub(crate) fn project(source: &str, key: &str, payload: &Value) -> Result<Fact> {
    let uri = reference(source, key)?;
    let (kind, date, titles): (&str, Option<i32>, &[&str]) = match source {
        "org.themoviedb" if key.starts_with("movie:") => (
            "movie",
            year(&payload["release_date"]),
            &["/title", "/original_title"],
        ),
        "org.themoviedb" if key.starts_with("tv:") => (
            "series",
            year(&payload["first_air_date"]),
            &["/name", "/original_name"],
        ),
        "com.thetvdb" if key.starts_with("series:") => (
            "series",
            year(
                payload
                    .pointer("/series/firstAired")
                    .unwrap_or(&payload["firstAired"]),
            ),
            if payload.get("series").is_some() {
                &["/series/name"]
            } else {
                &["/name"]
            },
        ),
        "com.thetvdb" if key.starts_with("movie:") => ("movie", year(&payload["year"]), &["/name"]),
        "co.anilist" => (
            if payload["format"] == "MOVIE" {
                "movie"
            } else if payload["type"] == "ANIME" {
                "series"
            } else {
                "manga"
            },
            payload
                .pointer("/startDate/year")
                .and_then(Value::as_i64)
                .and_then(|year| i32::try_from(year).ok()),
            &["/title/romaji", "/title/english", "/title/native"],
        ),
        _ => ("unit", None, &[]),
    };
    let titles = titles
        .iter()
        .filter_map(|field| {
            payload
                .pointer(field)
                .and_then(Value::as_str)
                .map(|value| ((*field).to_owned(), normalize(value)))
        })
        .filter(|(_, value)| !value.is_empty() && value.len() <= 512)
        .collect();
    let mut identifiers = Vec::new();
    if source == "org.themoviedb" {
        if matches!(kind, "movie" | "series")
            && let Some(id) = payload
                .pointer("/external_ids/imdb_id")
                .and_then(Value::as_str)
            && id.starts_with("tt")
            && id.len() > 2
            && id[2..].bytes().all(|byte| byte.is_ascii_digit())
        {
            identifiers.push((
                "/external_ids/imdb_id".into(),
                format!("com.imdb://title/{id}"),
            ));
        }
        if (key.starts_with("movie:") || key.starts_with("tv:"))
            && let Some(id) = payload
                .pointer("/external_ids/tvdb_id")
                .and_then(Value::as_u64)
                .filter(|id| *id > 0)
        {
            let kind = if key.starts_with("tv:") {
                "series"
            } else {
                "movie"
            };
            identifiers.push((
                "/external_ids/tvdb_id".into(),
                format!("com.thetvdb://{kind}/{id}"),
            ));
        }
    }
    if source == "co.anilist"
        && let Some(id) = payload["idMal"].as_u64().filter(|id| *id > 0)
    {
        identifiers.push((
            "/idMal".into(),
            format!(
                "net.myanimelist://{}/{id}",
                if payload["type"] == "MANGA" {
                    "manga"
                } else {
                    "anime"
                }
            ),
        ));
    }
    if source == "com.thetvdb" {
        let body = payload.get("series").unwrap_or(payload);
        for (position, remote) in body["remoteIds"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let id = remote["id"].as_str().unwrap_or("");
            if remote["sourceName"]
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case("imdb"))
                && id.starts_with("tt")
                && id.len() > 2
                && id[2..].bytes().all(|byte| byte.is_ascii_digit())
            {
                identifiers.push((
                    format!(
                        "{}/remoteIds/{position}/id",
                        if payload.get("series").is_some() {
                            "/series"
                        } else {
                            ""
                        }
                    ),
                    format!("com.imdb://title/{id}"),
                ));
            }
        }
    }
    ensure!(
        date.is_none_or(|year| (1..=9999).contains(&year)),
        "invalid native year"
    );
    Ok(Fact {
        uri,
        kind: kind.into(),
        year: date,
        titles,
        identifiers,
    })
}

impl Fact {
    pub fn blocks(&self) -> BTreeSet<String> {
        let mut blocks = BTreeSet::from([format!("id:{}", self.uri)]);
        blocks.extend(
            self.identifiers
                .iter()
                .map(|(_, target)| format!("id:{target}")),
        );
        if let Some(year) = self.year {
            blocks.extend(
                self.titles
                    .iter()
                    .map(|(_, title)| format!("title:{}:{year}:{title}", self.kind)),
            );
        }
        blocks
    }
}

pub(crate) fn normalize(text: &str) -> String {
    text.nfkc()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn year(value: &Value) -> Option<i32> {
    value
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .or_else(|| value.as_str()?.get(..4)?.parse().ok())
}

pub(crate) fn catalog(
    source: &str,
    records: &BTreeMap<String, Value>,
) -> Result<crate::adapters::Catalog> {
    use crate::adapters::{AniList, SourceAdapter, Tmdb, Tvdb};
    match source {
        "co.anilist" => AniList.prepare(source, records),
        "org.themoviedb" => Tmdb.prepare(source, records),
        "com.thetvdb" => Tvdb.prepare(source, records),
        _ => anyhow::bail!("no native selector adapter for {source}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn blocks_are_native_identifiers_or_normalized_typed_year_titles() {
        let fact = project("org.themoviedb", "movie:7", &json!({"id":7,"title":"A: Film!", "release_date":"2020-01-02", "external_ids":{"imdb_id":"tt123"}})).unwrap();
        assert!(fact.blocks().contains("title:movie:2020:a film"));
        assert!(fact.blocks().contains("id:com.imdb://title/tt123"));
        assert_eq!(fact.uri, "org.themoviedb://movie/7");
    }
}
