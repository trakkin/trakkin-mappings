use super::catalogue::{Reconciliation, select_domains};
use super::{Http, array, id};
use crate::{Change, Operation, Page, Provider};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

pub struct Tvdb {
    http: Http,
    token: String,
    endpoint: String,
    domains: BTreeSet<String>,
    reconciliation: Reconciliation,
}

fn next_page(result: &Value, page: u64) -> Result<Option<u64>> {
    let next = result
        .get("links")
        .and_then(|links| links.get("next"))
        .context("Missing TVDB pagination links")?;
    if next.is_null() || next.as_str() == Some("") {
        return Ok(None);
    }
    let link = next.as_str().context("Invalid TVDB next link")?;
    let base = reqwest::Url::parse("https://api4.thetvdb.com/v4/")?;
    let url = base.join(link)?;
    let next = url
        .query_pairs()
        .find(|(key, _)| key == "page")
        .context("Missing TVDB next page")?
        .1
        .parse::<u64>()?;
    ensure!(next > page, "TVDB pagination did not advance");
    Ok(Some(next))
}

impl Tvdb {
    pub const DOMAINS: &[&str] = &["movie", "series", "season", "episode"];

    fn single_domain(&self) -> Option<&str> {
        (self.domains.len() == 1).then(|| self.domains.first().unwrap().as_str())
    }

    fn scope(&self, mut page: Page) -> Page {
        page.changes.retain(|change| {
            change
                .key()
                .split_once(':')
                .is_some_and(|(domain, _)| self.domains.contains(domain))
        });
        page
    }

    pub fn new(api_key: String, pin: Option<String>, domains: &[&str]) -> Result<Self> {
        Self::with_endpoint(api_key, pin, domains, "https://api4.thetvdb.com/v4")
    }
    pub fn with_endpoint(
        api_key: String,
        pin: Option<String>,
        domains: &[&str],
        endpoint: &str,
    ) -> Result<Self> {
        let domains = select_domains(Self::DOMAINS, domains)?;
        ensure!(
            !api_key.is_empty(),
            "TRAKKIN_MAPPINGS_INGESTION_TVDB_API_KEY is required"
        );
        let http = Http::new(Duration::from_secs(1).div_f64(50.0))?;
        let mut body = json!({"apikey":api_key});
        if let Some(pin) = pin.filter(|pin| !pin.is_empty()) {
            body["pin"] = json!(pin);
        }
        let result = http.json(http.client.post(format!("{endpoint}/login")).json(&body))?;
        let token = result["data"]["token"]
            .as_str()
            .context("Missing TVDB login token")?
            .to_owned();
        Ok(Self {
            http,
            token,
            endpoint: endpoint.into(),
            domains,
            reconciliation: Reconciliation::default(),
        })
    }
    fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        let result = self.http.json(
            self.http
                .client
                .get(format!("{}{path}", self.endpoint))
                .bearer_auth(&self.token)
                .query(params),
        )?;
        ensure!(
            result["status"] == "success",
            "TVDB response was not successful"
        );
        Ok(result)
    }

    fn catalogue(&mut self, cursor: &Value) -> Result<Page> {
        let first = Self::DOMAINS
            .iter()
            .position(|domain| self.domains.contains(*domain))
            .unwrap();
        let kind = cursor["kind"].as_u64().unwrap_or(first as u64);
        let (media_type, prefix) = match kind {
            0 => ("movies", "movie"),
            1 => ("series", "series"),
            2 => ("seasons", "season"),
            3 => ("episodes", "episode"),
            _ => anyhow::bail!("Invalid TVDB catalogue cursor"),
        };
        ensure!(
            self.domains.contains(prefix),
            "TVDB catalogue cursor belongs to another domain"
        );
        let page = cursor["page"].as_u64().unwrap_or(0);
        let result = self.get(&format!("/{media_type}"), &[("page", page.to_string())])?;
        let changes = array(&result, "data")?
            .iter()
            .map(|record| Ok(Change::Dirty(format!("{prefix}:{}", id(record, "id")?))))
            .collect::<Result<_>>()?;
        let next = next_page(&result, page)?
            .map(|page| json!({"kind":kind,"page":page}))
            .or_else(|| {
                Self::DOMAINS
                    .iter()
                    .enumerate()
                    .skip(kind as usize + 1)
                    .find(|(_, domain)| self.domains.contains(**domain))
                    .map(|(kind, _)| json!({"kind":kind,"page":0}))
            });
        Ok(self.scope(Page { changes, next }))
    }

    fn changes(&mut self, watermark: i64, cursor: &Value) -> Result<Page> {
        if self.domains.contains("series")
            && let Some(refresh) = cursor.get("refresh")
        {
            return self.refresh_series(
                refresh,
                cursor
                    .get("updates")
                    .filter(|value| !value.is_null())
                    .cloned(),
            );
        }
        let page = cursor["page"].as_u64().unwrap_or(0);
        let mut params = vec![
            ("since", watermark.saturating_sub(86400).to_string()),
            ("page", page.to_string()),
        ];
        if let Some(domain) = self.single_domain().filter(|domain| *domain != "series") {
            let kind = match domain {
                "movie" => "movie",
                "season" => "seasons",
                "episode" => "episodes",
                _ => unreachable!(),
            };
            params.push(("type", kind.into()));
        }
        let result = self.get("/updates", &params)?;
        let mut changes = Vec::new();
        let mut refresh_all = false;
        let mut parents = BTreeMap::new();
        for update in array(&result, "data")? {
            if self.domains.contains("series")
                && (update["recordType"] == "seasontypes" || update["entityType"] == "seasontypes")
            {
                refresh_all = true;
                continue;
            }
            let Some(kind) = record_kind(
                update["recordType"]
                    .as_str()
                    .or_else(|| update["entityType"].as_str())
                    .unwrap_or(""),
            ) else {
                continue;
            };
            if self.domains.contains("series") && matches!(kind, "season" | "episode") {
                let key = format!("{kind}:{}", id(update, "recordId")?);
                let series_id =
                    if let Some(series_id) = update["seriesId"].as_u64().filter(|id| *id > 0) {
                        Some(series_id)
                    } else if let Some(parent) = parents.get(&key) {
                        Some(*parent)
                    } else if let Some(child) = self.fetch_record(&key)? {
                        let parent = id(&child, "seriesId")?;
                        parents.insert(key, parent);
                        Some(parent)
                    } else {
                        None
                    };
                if let Some(series_id) = series_id {
                    changes.push(Change::Dirty(format!("series:{series_id}")));
                } else {
                    refresh_all = true;
                }
                if self.single_domain() == Some("series") {
                    continue;
                }
            }
            let key = format!("{kind}:{}", id(update, "recordId")?);
            let method = update["methodInt"]
                .as_u64()
                .context("Missing TVDB update method")?;
            ensure!((1..=3).contains(&method), "Unknown TVDB update method");
            if method == 3 {
                changes.push(Change::Deleted {
                    key,
                    metadata: update.clone(),
                });
                if let Some(target) = update["mergeToId"].as_u64().filter(|target| *target > 0)
                    && let Some(kind) =
                        record_kind(update["mergeToEntityType"].as_str().unwrap_or(""))
                {
                    changes.push(Change::Dirty(format!("{kind}:{target}")));
                }
            } else {
                changes.push(Change::Dirty(key));
            }
        }
        let next = next_page(&result, page)?.map(|page| json!({"page":page}));
        if refresh_all {
            let mut refresh = self.refresh_series(&Value::Null, next)?;
            changes.append(&mut refresh.changes);
            return Ok(self.scope(Page {
                changes,
                next: refresh.next,
            }));
        }
        Ok(self.scope(Page { changes, next }))
    }

    fn refresh_series(&mut self, cursor: &Value, updates: Option<Value>) -> Result<Page> {
        let cursor = if cursor.is_null() {
            json!({"kind":1,"page":0})
        } else {
            cursor.clone()
        };
        let mut page = self.catalogue(&cursor)?;
        if page.next.as_ref().is_some_and(|next| next["kind"] != 1) {
            page.next = None;
        }
        page.next = match page.next {
            Some(next) => Some(json!({"refresh":next,"updates":updates})),
            None => updates,
        };
        Ok(page)
    }

    fn fetch_record(&self, key: &str) -> Result<Option<Value>> {
        let (kind, record_id) = key.split_once(':').context("Invalid TVDB key")?;
        let path = match kind {
            "movie" => "movies",
            "series" => "series",
            "season" => "seasons",
            "episode" => "episodes",
            _ => anyhow::bail!("Invalid TVDB record type"),
        };
        let record_id: u64 = record_id.parse()?;
        let response = self.http.send(
            self.http
                .client
                .get(format!("{}/{path}/{record_id}/extended", self.endpoint))
                .bearer_auth(&self.token),
        )?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        let result: Value = response.json()?;
        ensure!(
            result["status"] == "success",
            "TVDB fetch was not successful"
        );
        let record = result.get("data").context("Missing TVDB record data")?;
        ensure!(
            id(record, "id")? == record_id,
            "TVDB returned a different record"
        );
        if kind == "series" {
            let mut types = std::collections::BTreeSet::new();
            let default_type = id(record, "defaultSeasonType")?;
            let season_types = array(record, "seasonTypes")?;
            ensure!(
                season_types.is_empty()
                    || season_types
                        .iter()
                        .any(|season_type| season_type["id"].as_u64() == Some(default_type)),
                "TVDB default season type is missing from available orders"
            );
            for season_type in season_types {
                let order = season_type["type"]
                    .as_str()
                    .context("Missing TVDB season type")?;
                ensure!(
                    !order.is_empty()
                        && order.bytes().all(|byte| byte.is_ascii_alphanumeric()
                            || byte == b'-'
                            || byte == b'_'),
                    "Invalid TVDB season type"
                );
                ensure!(types.insert(order.to_owned()), "Duplicate TVDB season type");
            }
            let types: Vec<_> = types.into_iter().collect();
            let orders: BTreeMap<_, _> = types
                .iter()
                .map(|order| {
                    let mut episodes = Vec::new();
                    let mut page = 0;
                    loop {
                        let result = self.get(
                            &format!("/series/{record_id}/episodes/{order}"),
                            &[("page", page.to_string())],
                        )?;
                        ensure!(
                            id(&result["data"]["series"], "id")? == record_id,
                            "TVDB order belongs to another series"
                        );
                        for episode in array(&result["data"], "episodes")? {
                            ensure!(
                                id(episode, "seriesId")? == record_id,
                                "TVDB ordered episode belongs to another series"
                            );
                            episodes.push(episode.clone());
                        }
                        let Some(next) = next_page(&result, page)? else {
                            break;
                        };
                        page = next;
                    }
                    let episodes = filter_ordered_episodes(episodes, record_id, order)?;
                    Ok((order.clone(), episodes))
                })
                .collect::<Result<_>>()?;
            return Ok(Some(json!({"series":record,"episode_orders":orders})));
        }
        Ok(Some(record.clone()))
    }
}

impl Provider for Tvdb {
    fn source(&self) -> &'static str {
        "com.thetvdb"
    }
    fn restore(
        &mut self,
        operation: Operation,
        index: &BTreeMap<String, crate::storage::Entry>,
        _checkpoint: &Value,
    ) -> Result<()> {
        self.reconciliation =
            Reconciliation::restore(operation, index, |domain| self.domains.contains(domain));
        Ok(())
    }
    fn discover(
        &mut self,
        operation: Operation,
        watermark: Option<i64>,
        cursor: &Value,
        _until: i64,
    ) -> Result<Page> {
        if operation == Operation::Sync {
            return self.changes(watermark.context("Missing TVDB watermark")?, cursor);
        }
        if operation == Operation::Reconcile
            && let Some(page) = self.reconciliation.absences(cursor, Change::Dirty)?
        {
            return Ok(page);
        }
        let mut page = self.catalogue(cursor)?;
        if operation == Operation::Reconcile {
            self.reconciliation.observe(&mut page)?;
        }
        Ok(page)
    }
    fn fetch(&mut self, key: &str) -> Result<Option<Value>> {
        self.fetch_record(key)
    }

    fn fetch_many(&mut self, keys: &[String]) -> Result<BTreeMap<String, Option<Value>>> {
        super::fetch_many(keys, 12, |key| self.fetch_record(key))
    }
}

fn filter_ordered_episodes(
    episodes: Vec<Value>,
    series_id: u64,
    order: &str,
) -> Result<Vec<Value>> {
    let mut groups = BTreeMap::<u64, Vec<usize>>::new();
    for (index, episode) in episodes.iter().enumerate() {
        groups.entry(id(episode, "id")?).or_default().push(index);
    }
    let mut retained = std::collections::BTreeSet::new();
    let mut skipped = Vec::new();
    for (episode_id, indices) in groups {
        let first = &episodes[indices[0]];
        if indices.iter().all(|index| episodes[*index] == *first) {
            retained.insert(indices[0]);
        } else {
            skipped.push(episode_id);
        }
    }
    if !skipped.is_empty() {
        eprintln!(
            "Warning: skipping {} TVDB episode IDs with conflicting entries for series {series_id}, order {order}: {skipped:?}",
            skipped.len()
        );
    }
    Ok(episodes
        .into_iter()
        .enumerate()
        .filter(|(index, _)| retained.contains(index))
        .map(|(_, episode)| episode)
        .collect())
}

fn record_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "movie" | "movies" => Some("movie"),
        "series" => Some("series"),
        "season" | "seasons" => Some("season"),
        "episode" | "episodes" => Some("episode"),
        _ => None,
    }
}
