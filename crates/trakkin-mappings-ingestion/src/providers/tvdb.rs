use super::{Http, array, id};
use crate::{Change, Page, Provider};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

pub struct Tvdb {
    http: Http,
    token: String,
    endpoint: String,
    domain: Option<u64>,
    series_selected: bool,
}

impl Tvdb {
    pub fn new(api_key: String, pin: Option<String>) -> Result<Self> {
        Self::with_endpoint(api_key, pin, "https://api4.thetvdb.com/v4")
    }
    pub fn with_endpoint(api_key: String, pin: Option<String>, endpoint: &str) -> Result<Self> {
        ensure!(
            !api_key.is_empty(),
            "TRAKKIN_MAPPINGS_INGESTION_TVDB_API_KEY is required"
        );
        let http = Http::new(Duration::from_millis(25))?;
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
            domain: None,
            series_selected: true,
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

impl Provider for Tvdb {
    fn source(&self) -> Option<&'static str> {
        Some("com.thetvdb")
    }
    fn select_domain(&mut self, domain: &str) {
        self.series_selected = domain == "series";
        self.domain = match domain {
            "movie" => Some(0),
            "series" => Some(1),
            "season" => Some(2),
            "episode" => Some(3),
            _ => None,
        };
    }
    fn select_domains(&mut self, domains: &[&str]) {
        if let [domain] = domains {
            self.select_domain(domain);
        } else {
            self.domain = None;
            self.series_selected = domains.contains(&"series");
        }
    }

    fn enumerate(&mut self, cursor: &Value, _started: i64) -> Result<Page> {
        let kind = cursor["kind"].as_u64().unwrap_or(self.domain.unwrap_or(0));
        let (media_type, prefix) = match kind {
            0 => ("movies", "movie"),
            1 => ("series", "series"),
            2 => ("seasons", "season"),
            3 => ("episodes", "episode"),
            _ => anyhow::bail!("Invalid TVDB catalogue cursor"),
        };
        let page = cursor["page"].as_u64().unwrap_or(0);
        let result = self.get(&format!("/{media_type}"), &[("page", page.to_string())])?;
        let changes = array(&result, "data")?
            .iter()
            .map(|record| Ok(Change::Dirty(format!("{prefix}:{}", id(record, "id")?))))
            .collect::<Result<_>>()?;
        let next = next_page(&result, page)?
            .map(|page| json!({"kind":kind,"page":page}))
            .or_else(|| {
                (self.domain.is_none() && kind < 3).then(|| json!({"kind":kind+1,"page":0}))
            });
        Ok(Page { changes, next })
    }

    fn discover_changes(&mut self, watermark: i64, cursor: &Value, until: i64) -> Result<Page> {
        if self.series_selected
            && let Some(refresh) = cursor.get("refresh")
        {
            return self.refresh_series(
                refresh,
                until,
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
        if let Some(domain) = self.domain.filter(|domain| *domain != 1) {
            let kind = match domain {
                0 => "movie",
                1 => "series",
                2 => "seasons",
                3 => "episodes",
                _ => unreachable!(),
            };
            params.push(("type", kind.into()));
        }
        let result = self.get("/updates", &params)?;
        let mut changes = Vec::new();
        let mut refresh_all = false;
        let mut parents = BTreeMap::new();
        for update in array(&result, "data")? {
            if self.series_selected
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
            if self.series_selected && matches!(kind, "season" | "episode") {
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
                if self.domain == Some(1) {
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
            let mut refresh = self.refresh_series(&Value::Null, until, next)?;
            changes.append(&mut refresh.changes);
            return Ok(Page {
                changes,
                next: refresh.next,
            });
        }
        Ok(Page { changes, next })
    }

    fn fetch(&mut self, key: &str) -> Result<Option<Value>> {
        self.fetch_record(key)
    }

    fn fetch_many(&mut self, keys: &[String]) -> Result<BTreeMap<String, Option<Value>>> {
        super::fetch_many(keys, 12, |key| self.fetch_record(key))
    }
}

impl Tvdb {
    fn refresh_series(
        &mut self,
        cursor: &Value,
        until: i64,
        updates: Option<Value>,
    ) -> Result<Page> {
        let cursor = if cursor.is_null() {
            json!({"kind":1,"page":0})
        } else {
            cursor.clone()
        };
        let mut page = self.enumerate(&cursor, until)?;
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
                    let mut seen = std::collections::BTreeSet::new();
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
                            ensure!(
                                seen.insert(id(episode, "id")?),
                                "Duplicate TVDB ordered episode"
                            );
                            episodes.push(episode.clone());
                        }
                        let Some(next) = next_page(&result, page)? else {
                            break;
                        };
                        page = next;
                    }
                    Ok((order.clone(), episodes))
                })
                .collect::<Result<_>>()?;
            return Ok(Some(json!({"series":record,"episode_orders":orders})));
        }
        Ok(Some(record.clone()))
    }
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
