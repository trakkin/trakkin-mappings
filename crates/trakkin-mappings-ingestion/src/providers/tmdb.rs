use super::{Http, array, id};
use crate::{Change, Page, Provider};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Duration as Days};
use flate2::read::GzDecoder;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufRead, BufReader},
    time::Duration,
};
use tempfile::NamedTempFile;

const EXPORTS: [(&str, &str); 4] = [
    ("movie", "movie_ids"),
    ("tv", "tv_series_ids"),
    ("movie", "adult_movie_ids"),
    ("tv", "adult_tv_series_ids"),
];

pub struct Tmdb {
    http: Http,
    token: String,
    endpoint: String,
    exports: String,
    spool: Option<(usize, NamedTempFile, BufReader<File>)>,
    domains: BTreeSet<String>,
    metadata: BTreeMap<String, Value>,
    acquired: BTreeMap<String, Option<Value>>,
}

impl Tmdb {
    fn catalogue_route(&self) -> (usize, usize) {
        if self.domains.contains("movie") {
            if self.domains.len() == 1 {
                (0, 2)
            } else {
                (0, 1)
            }
        } else {
            (1, 2)
        }
    }

    fn hierarchy(&self) -> bool {
        ["season", "episode", "episode_group"]
            .iter()
            .any(|domain| self.domains.contains(*domain))
    }

    pub fn new(token: String) -> Result<Self> {
        Self::with_endpoints(
            token,
            "https://api.themoviedb.org/3",
            "https://files.tmdb.org/p/exports",
        )
    }
    pub fn with_endpoints(token: String, endpoint: &str, exports: &str) -> Result<Self> {
        ensure!(
            !token.is_empty(),
            "TRAKKIN_MAPPINGS_INGESTION_TMDB_TOKEN is required"
        );
        Ok(Self {
            http: Http::new(Duration::from_secs(1).div_f64(35.0))?,
            token,
            endpoint: endpoint.into(),
            exports: exports.into(),
            spool: None,
            domains: ["movie".into(), "tv".into()].into(),
            metadata: BTreeMap::new(),
            acquired: BTreeMap::new(),
        })
    }
    fn export(&mut self, number: usize, started: i64) -> Result<()> {
        if self
            .spool
            .as_ref()
            .is_some_and(|(current, _, _)| *current == number)
        {
            return Ok(());
        }
        let date = DateTime::from_timestamp(started, 0)
            .context("Invalid export timestamp")?
            .date_naive()
            - Days::days(1);
        let url = format!(
            "{}/{}_{}.json.gz",
            self.exports,
            EXPORTS[number].1,
            date.format("%m_%d_%Y")
        );
        let response = self.http.send(self.http.client.get(url))?;
        ensure!(
            response.status().is_success(),
            "TMDB daily export is unavailable; no deletions committed"
        );
        let mut file = NamedTempFile::new()?;
        std::io::copy(&mut GzDecoder::new(response), file.as_file_mut())
            .context("Download and verify TMDB gzip export")?;
        let reader = BufReader::new(file.reopen()?);
        self.spool = Some((number, file, reader));
        Ok(())
    }
}

impl Provider for Tmdb {
    fn source(&self) -> Option<&'static str> {
        Some("org.themoviedb")
    }
    fn restore(&mut self, index: &BTreeMap<String, crate::storage::Entry>) -> Result<()> {
        self.metadata = index
            .iter()
            .map(|(key, entry)| (key.clone(), entry.metadata.clone()))
            .collect();
        Ok(())
    }
    fn metadata(&self, key: &str) -> Value {
        self.metadata.get(key).cloned().unwrap_or(json!({}))
    }
    fn confirm_absence(&self, key: &str) -> bool {
        matches!(
            key.split_once(':').map(|(domain, _)| domain),
            Some("movie" | "tv")
        )
    }
    fn select_domain(&mut self, domain: &str) {
        self.select_domains(&[domain]);
    }
    fn select_domains(&mut self, domains: &[&str]) {
        self.domains = domains.iter().map(|domain| (*domain).into()).collect();
    }

    fn enumerate(&mut self, cursor: &Value, started: i64) -> Result<Page> {
        self.acquired.clear();
        let (first, step) = self.catalogue_route();
        let number = cursor["export"].as_u64().unwrap_or(first as u64) as usize;
        ensure!(number < EXPORTS.len(), "Invalid TMDB export cursor");
        let children = self.hierarchy() && EXPORTS[number].0 == "tv";
        ensure!(
            step == 1 || number % 2 == first,
            "TMDB export cursor belongs to another domain"
        );
        let opened = !self
            .spool
            .as_ref()
            .is_some_and(|(current, _, _)| *current == number);
        self.export(number, started)?;
        let reader = &mut self.spool.as_mut().unwrap().2;
        if opened {
            for _ in 0..cursor["offset"].as_u64().unwrap_or(0) {
                let mut line = String::new();
                ensure!(
                    reader.read_line(&mut line)? > 0,
                    "TMDB export cursor exceeds catalogue"
                );
            }
        }
        let mut changes = Vec::new();
        let mut exhausted = false;
        for _ in 0..if children { 1 } else { 1000 } {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                exhausted = true;
                break;
            }
            let record: Value = serde_json::from_str(&line).context("Invalid TMDB export line")?;
            changes.push(Change::Dirty(format!(
                "{}:{}",
                EXPORTS[number].0,
                id(&record, "id")?
            )));
        }
        let offset = cursor["offset"].as_u64().unwrap_or(0) + changes.len() as u64;
        let next = if !exhausted {
            Some(json!({"export":number,"offset":offset}))
        } else if number + step < EXPORTS.len() {
            Some(json!({"export":number+step,"offset":0}))
        } else {
            None
        };
        let changes = if children {
            self.children(changes, true)?
        } else {
            changes
        };
        Ok(Page { changes, next })
    }

    fn discover_changes(&mut self, watermark: i64, cursor: &Value, until: i64) -> Result<Page> {
        self.acquired.clear();
        let (first, _) = self.catalogue_route();
        let kind = cursor["kind"].as_u64().unwrap_or(first as u64);
        ensure!(kind < 2, "Invalid TMDB change cursor");
        ensure!(
            self.domains.len() > 1 && self.domains.contains("movie") || kind == first as u64,
            "TMDB change cursor belongs to another domain"
        );
        let media_type = if kind == 0 { "movie" } else { "tv" };
        let children = self.hierarchy() && kind == 1;
        let page = cursor["page"].as_u64().unwrap_or(1);
        let start = cursor["start"]
            .as_i64()
            .unwrap_or(watermark.saturating_sub(86400));
        let start_date = DateTime::from_timestamp(start, 0)
            .context("Invalid TMDB start timestamp")?
            .date_naive();
        let until_date = DateTime::from_timestamp(until, 0)
            .context("Invalid TMDB end timestamp")?
            .date_naive();
        let end_date = (start_date + Days::days(12)).min(until_date);
        ensure!(start_date <= end_date, "TMDB watermark is in the future");
        let request = self
            .http
            .client
            .get(format!("{}/{media_type}/changes", self.endpoint))
            .bearer_auth(&self.token)
            .query(&[
                ("page", page.to_string()),
                ("start_date", start_date.to_string()),
                ("end_date", end_date.to_string()),
            ]);
        let result = self.http.json(request)?;
        let changes = array(&result, "results")?
            .iter()
            .map(|record| Ok(Change::Dirty(format!("{media_type}:{}", id(record, "id")?))))
            .collect::<Result<_>>()?;
        let total = result["total_pages"]
            .as_u64()
            .context("Missing TMDB total_pages")?;
        ensure!(
            total <= 500,
            "TMDB change window exceeds pagination limit; use a smaller window"
        );
        let next = if page < total {
            Some(json!({"kind":kind,"page":page+1,"start":start}))
        } else if end_date < until_date {
            Some(
                json!({"kind":kind,"page":1,"start":end_date.and_hms_opt(0,0,0).unwrap().and_utc().timestamp()}),
            )
        } else if kind == 0 && self.domains.len() > 1 {
            Some(json!({"kind":1,"page":1,"start":watermark.saturating_sub(86400)}))
        } else {
            None
        };
        let changes = if children {
            self.children(changes, false)?
        } else {
            changes
        };
        Ok(Page { changes, next })
    }

    fn fetch(&mut self, key: &str) -> Result<Option<Value>> {
        if let Some(record) = self.acquired.get(key) {
            return Ok(record.clone());
        }
        let address = self
            .metadata
            .get(key)
            .and_then(|metadata| metadata["address"].as_str())
            .unwrap_or(key);
        let record = fetch_record(&self.http, &self.token, &self.endpoint, address)?;
        if address != key
            && let Some(record) = &record
        {
            let expected: u64 = key.split_once(':').context("Invalid TMDB key")?.1.parse()?;
            ensure!(
                id(record, "id")? == expected,
                "TMDB address now belongs to another entity"
            );
        }
        Ok(record)
    }

    fn fetch_many(&mut self, keys: &[String]) -> Result<BTreeMap<String, Option<Value>>> {
        super::fetch_many(keys, 16, |key| {
            if let Some(record) = self.acquired.get(key) {
                return Ok(record.clone());
            }
            let address = self
                .metadata
                .get(key)
                .and_then(|metadata| metadata["address"].as_str())
                .unwrap_or(key);
            let record = fetch_record(&self.http, &self.token, &self.endpoint, address)?;
            if address != key
                && let Some(record) = &record
            {
                let expected: u64 = key.split_once(':').context("Invalid TMDB key")?.1.parse()?;
                ensure!(
                    id(record, "id")? == expected,
                    "TMDB address now belongs to another entity"
                );
            }
            Ok(record)
        })
    }
}

fn fetch_record(http: &Http, token: &str, endpoint: &str, key: &str) -> Result<Option<Value>> {
    let (kind, record_id) = key.split_once(':').context("Invalid TMDB key")?;
    if kind == "episode_group" {
        ensure!(
            !record_id.is_empty() && record_id.bytes().all(|byte| byte.is_ascii_alphanumeric()),
            "Invalid TMDB episode group ID"
        );
        let response = http.send(
            http.client
                .get(format!("{endpoint}/tv/episode_group/{record_id}"))
                .bearer_auth(token),
        )?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        let record: Value = response.json()?;
        ensure!(
            record["id"].as_str() == Some(record_id),
            "TMDB returned a different episode group"
        );
        array(&record, "groups")?;
        return Ok(Some(record));
    }
    let parts = record_id
        .split('/')
        .map(str::parse::<u64>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let path = match (kind, parts.as_slice()) {
        ("movie" | "tv", [record_id]) if *record_id > 0 => format!("{kind}/{record_id}"),
        ("season", [show, season]) if *show > 0 => format!("tv/{show}/season/{season}"),
        ("episode", [show, season, episode]) if *show > 0 && *episode > 0 => {
            format!("tv/{show}/season/{season}/episode/{episode}")
        }
        _ => anyhow::bail!("Invalid TMDB record key"),
    };
    let response = http.send(
        http.client
            .get(format!("{endpoint}/{path}"))
            .bearer_auth(token)
            .query(&[("append_to_response", "external_ids")]),
    )?;
    if response.status().as_u16() == 404 {
        return Ok(None);
    }
    let record: Value = response.json()?;
    let native_id = id(&record, "id")?;
    match parts.as_slice() {
        [expected] => ensure!(native_id == *expected, "TMDB returned a different record"),
        [_, season] => ensure!(
            record["season_number"].as_u64() == Some(*season),
            "TMDB returned a different season"
        ),
        [_, season, episode] => ensure!(
            record["season_number"].as_u64() == Some(*season)
                && record["episode_number"].as_u64() == Some(*episode),
            "TMDB returned a different episode"
        ),
        _ => unreachable!(),
    }
    Ok(Some(record))
}

impl Tmdb {
    fn children(&mut self, parents: Vec<Change>, catalogue: bool) -> Result<Vec<Change>> {
        let mut changes = Vec::new();
        for parent in parents {
            let Change::Dirty(key) = parent else { continue };
            let show = key.strip_prefix("tv:").context("Expected TMDB show")?;
            let parent = key.clone();
            let mut members = BTreeSet::new();
            let Some(record) = self.fetch(&key)? else {
                ensure!(!catalogue, "TMDB catalogue contains an unavailable show");
                changes.push(Change::Membership {
                    parent,
                    keys: members,
                });
                if self.domains.contains("tv") {
                    self.acquired.insert(key.clone(), None);
                    changes.push(Change::Dirty(key));
                }
                continue;
            };
            if self.domains.contains("tv") {
                self.acquired.insert(key.clone(), Some(record.clone()));
                changes.push(Change::Dirty(key.clone()));
            }
            if self.domains.contains("episode_group") {
                let listing = self.http.json(
                    self.http
                        .client
                        .get(format!("{}/tv/{show}/episode_groups", self.endpoint))
                        .bearer_auth(&self.token),
                )?;
                for group in array(&listing, "results")? {
                    let group_id = group["id"]
                        .as_str()
                        .context("Missing TMDB episode group ID")?;
                    ensure!(
                        !group_id.is_empty()
                            && group_id.bytes().all(|byte| byte.is_ascii_alphanumeric()),
                        "Invalid TMDB episode group ID"
                    );
                    let key = format!("episode_group:{group_id}");
                    self.metadata.insert(key.clone(), json!({"parent":parent}));
                    members.insert(key.clone());
                    changes.push(Change::Dirty(key));
                }
            }
            let seasons = if self.domains.contains("season") || self.domains.contains("episode") {
                array(&record, "seasons")?.as_slice()
            } else {
                &[]
            };
            let details = if !seasons.is_empty() {
                super::parallel_map(seasons, 16, |season| {
                    let number = season["season_number"]
                        .as_u64()
                        .context("Missing TMDB season number")?;
                    let record = fetch_record(
                        &self.http,
                        &self.token,
                        &self.endpoint,
                        &format!("season:{show}/{number}"),
                    )?
                    .context("TMDB listed season is unavailable")?;
                    ensure!(
                        id(&record, "id")? == id(season, "id")?,
                        "TMDB season identity changed during acquisition"
                    );
                    Ok(record)
                })?
            } else {
                Vec::new()
            };
            for (position, season) in seasons.iter().enumerate() {
                let number = season["season_number"]
                    .as_u64()
                    .context("Missing TMDB season number")?;
                let season_id = id(season, "id")?;
                let key = format!("season:{show}/{number}");
                if self.domains.contains("season") {
                    let native = format!("season:{season_id}");
                    self.metadata
                        .insert(native.clone(), json!({"address":key,"parent":parent}));
                    members.insert(native.clone());
                    self.acquired
                        .insert(native.clone(), Some(details[position].clone()));
                    changes.push(Change::Dirty(native));
                }
                if self.domains.contains("episode") {
                    let season = details
                        .get(position)
                        .context("Missing TMDB season acquisition")?;
                    for episode in array(season, "episodes")? {
                        let episode_id = id(episode, "id")?;
                        let episode_number = id(episode, "episode_number")?;
                        ensure!(
                            episode["season_number"].as_u64() == Some(number)
                                && episode["show_id"].as_u64() == show.parse::<u64>().ok(),
                            "TMDB episode belongs to another season or show"
                        );
                        let native = format!("episode:{episode_id}");
                        self.metadata.insert(native.clone(), json!({"address":format!("episode:{show}/{number}/{episode_number}"),"parent":parent,"season_id":season_id}));
                        members.insert(native.clone());
                        changes.push(Change::Dirty(native));
                    }
                }
            }
            changes.push(Change::Membership {
                parent,
                keys: members,
            });
        }
        Ok(changes)
    }
}
