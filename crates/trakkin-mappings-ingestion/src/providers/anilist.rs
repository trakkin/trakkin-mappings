use super::{Http, array, id};
use crate::{Change, Page, Provider};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

const MEDIA_FIELDS: &str = "id idMal title{romaji english native} type format status description startDate{year month day} endDate{year month day} season seasonYear episodes duration chapters volumes countryOfOrigin source updatedAt genres synonyms isAdult siteUrl relations{edges{id relationType(version:3) node{id type}}}";

const CATALOGUE_PAGES: usize = 99;
const CHANGES_PAGES: usize = 83;
const PAYLOAD_PAGES: usize = 11;

fn page_name(offset: usize) -> String {
    if offset == 0 {
        "Page".into()
    } else {
        format!("page{offset}")
    }
}

fn pages_query(count: usize, arguments: &str, fields: &str) -> String {
    let mut query = "query($page:Int!".to_owned();
    if arguments.contains("$ids") {
        query.push_str(",$ids:[Int]");
    }
    if arguments.contains("$sort") {
        query.push_str(",$sort:[MediaSort]");
    }
    for offset in 1..count {
        query.push_str(&format!(",$page{offset}:Int!"));
    }
    query.push_str("){ ");
    if fields == "id" {
        query.push_str("latest:Media(sort:ID_DESC){id} ");
    }
    for offset in 0..count {
        let name = page_name(offset);
        let alias = if offset == 0 {
            String::new()
        } else {
            format!("{name}:")
        };
        let page = if offset == 0 { "page".into() } else { name };
        query.push_str(&format!("{alias}Page(page:${page},perPage:50){{pageInfo{{hasNextPage}} media({arguments}){{{fields}}}}}"));
    }
    query.push('}');
    query
}

pub struct AniList {
    http: Http,
    endpoint: String,
}

impl AniList {
    pub fn new() -> Result<Self> {
        Self::with_endpoint("https://graphql.anilist.co")
    }
    pub fn with_endpoint(endpoint: &str) -> Result<Self> {
        Ok(Self {
            http: Http::new(Duration::from_secs(2))?,
            endpoint: endpoint.into(),
        })
    }
    fn query(&mut self, query: &str, variables: Value, missing_media: bool) -> Result<Value> {
        let request = self
            .http
            .client
            .post(&self.endpoint)
            .json(&json!({"query":query,"variables":variables}));
        let response = self.http.send_with_status(request, &[400])?;
        let status = response.status();
        let result: Value = response.json().context("Decode AniList JSON")?;
        if (status.is_success() || status.as_u16() == 400)
            && result["errors"].as_array().is_some_and(|errors| {
                !errors.is_empty()
                    && errors.iter().all(|error| {
                        error["message"].as_str().is_some_and(|message| {
                            let message = message.to_ascii_lowercase();
                            message.contains("query is too complex")
                                || message.contains("max query complexity")
                        })
                    })
            })
        {
            anyhow::bail!("AniList query complexity limit");
        }
        if missing_media
            && status.as_u16() == 404
            && result["data"].get("Media").is_some_and(Value::is_null)
            && result["errors"].as_array().is_some_and(|errors| {
                !errors.is_empty() && errors.iter().all(|error| error["status"] == 404)
            })
        {
            return Ok(result["data"].clone());
        }
        ensure!(status.is_success(), "AniList GraphQL request failed");
        ensure!(
            result.get("errors").is_none(),
            "AniList GraphQL errors (body suppressed)"
        );
        result.get("data").cloned().context("Missing AniList data")
    }
    fn pages(
        &mut self,
        count: usize,
        page: u64,
        arguments: &str,
        fields: &str,
        mut variables: Value,
    ) -> Result<Value> {
        variables["page"] = json!(page);
        for offset in 1..count {
            variables[format!("page{offset}")] = json!(page + offset as u64);
        }
        self.query(&pages_query(count, arguments, fields), variables, false)
    }
    fn page(&mut self, cursor: &Value, watermark: Option<i64>) -> Result<Page> {
        let page = if watermark.is_some() {
            cursor["page"].as_u64().unwrap_or(1)
        } else {
            1
        };
        ensure!(
            page <= 100,
            "AniList changes exceed pagination depth; reconcile the catalogue"
        );
        let after = cursor["after"].as_u64().unwrap_or(0);
        let sort = if watermark.is_some() {
            "UPDATED_AT_DESC"
        } else {
            "ID"
        };
        ensure!(after <= i32::MAX as u64, "Invalid AniList catalogue cursor");
        let end = after.saturating_add(5000).min(i32::MAX as u64);
        let ids: Vec<_> = (after + 1..=end).collect();
        let count = if watermark.is_some() {
            CHANGES_PAGES.min((101 - page) as usize)
        } else {
            CATALOGUE_PAGES
        };
        let response = self.pages(
            count,
            page,
            if watermark.is_some() {
                "sort:$sort"
            } else {
                "id_in:$ids,sort:$sort"
            },
            if watermark.is_some() {
                "id updatedAt"
            } else {
                "id"
            },
            json!({"ids":ids,"sort":[sort,"ID"]}),
        )?;
        let mut changes = Vec::new();
        let mut reached = false;
        let mut last_id = after;
        let mut seen = std::collections::BTreeSet::new();
        let mut has_next = false;
        let mut consumed = 0;
        for offset in 0..count {
            let result = &response[page_name(offset)];
            let records = array(result, "media")?;
            has_next = result["pageInfo"]["hasNextPage"]
                .as_bool()
                .context("Missing AniList pageInfo")?;
            ensure!(
                !records.is_empty() || !has_next,
                "Empty AniList page with successor"
            );
            for record in records {
                let media_id = id(record, "id")?;
                ensure!(seen.insert(media_id), "Duplicate AniList catalogue ID");
                if let Some(watermark) = watermark {
                    let updated = record["updatedAt"]
                        .as_i64()
                        .context("Missing AniList updatedAt")?;
                    if updated < watermark.saturating_sub(86400) {
                        reached = true;
                        break;
                    }
                } else {
                    ensure!(
                        media_id > last_id && media_id <= end,
                        "AniList catalogue IDs did not advance within window"
                    );
                    last_id = media_id;
                }
                changes.push(Change::Dirty(format!("media:{media_id}")));
            }
            consumed += 1;
            if reached || !has_next {
                break;
            }
        }
        ensure!(
            watermark.is_none() || reached || !has_next || page + consumed <= 100,
            "AniList changes exceed pagination depth; reconcile the catalogue"
        );
        let next = if watermark.is_some() {
            (!reached && has_next).then(|| json!({"page":page+consumed}))
        } else {
            let latest = response
                .get("latest")
                .context("Missing AniList latest ID")?;
            let latest = if latest.is_null() {
                0
            } else {
                id(latest, "id")?
            };
            let after = if has_next { last_id } else { end };
            (after < latest).then(|| json!({"after":after}))
        };
        Ok(Page { changes, next })
    }
}

impl Provider for AniList {
    fn source(&self) -> Option<&'static str> {
        Some("co.anilist")
    }
    fn enumerate(&mut self, cursor: &Value, _started: i64) -> Result<Page> {
        self.page(cursor, None)
    }
    fn discover_changes(&mut self, watermark: i64, cursor: &Value, _until: i64) -> Result<Page> {
        self.page(cursor, Some(watermark))
    }
    fn restart_changes(&self) -> bool {
        true
    }
    fn fetch(&mut self, key: &str) -> Result<Option<Value>> {
        let media_id: u64 = key
            .strip_prefix("media:")
            .context("Invalid AniList key")?
            .parse()?;
        let response = self.query(
            &format!("query($id:Int!){{Media(id:$id){{{MEDIA_FIELDS}}}}}"),
            json!({"id":media_id}),
            true,
        )?;
        let record = response
            .get("Media")
            .context("Missing AniList Media field")?;
        if record.is_null() {
            return Ok(None);
        }
        ensure!(
            id(record, "id")? == media_id,
            "AniList returned a different record"
        );
        Ok(Some(record.clone()))
    }
    fn fetch_many(&mut self, keys: &[String]) -> Result<BTreeMap<String, Option<Value>>> {
        let mut records = BTreeMap::new();
        for chunk in keys.chunks(5000) {
            let ids: Vec<u64> = chunk
                .iter()
                .map(|key| {
                    let media_id = key
                        .strip_prefix("media:")
                        .context("Invalid AniList key")?
                        .parse()?;
                    ensure!(
                        media_id > 0 && media_id <= i32::MAX as u64,
                        "Invalid AniList ID"
                    );
                    Ok(media_id)
                })
                .collect::<Result<_>>()?;
            let mut page = 1;
            while page <= 100 {
                let count = PAYLOAD_PAGES
                    .min(ids.len().div_ceil(50))
                    .min((101 - page) as usize);
                let response = self.pages(
                    count,
                    page,
                    "id_in:$ids,sort:ID",
                    MEDIA_FIELDS,
                    json!({"ids":ids}),
                )?;
                let mut has_next = false;
                for offset in 0..count {
                    let result = &response[page_name(offset)];
                    has_next = result["pageInfo"]["hasNextPage"]
                        .as_bool()
                        .context("Missing AniList batch pageInfo")?;
                    let media = array(result, "media")?;
                    for record in media {
                        let media_id = id(record, "id")?;
                        ensure!(
                            ids.contains(&media_id),
                            "AniList returned an unrequested record"
                        );
                        ensure!(
                            records
                                .insert(format!("media:{media_id}"), Some(record.clone()))
                                .is_none(),
                            "Duplicate AniList record"
                        );
                    }
                    if chunk.iter().all(|key| records.contains_key(key)) || !has_next {
                        break;
                    }
                    ensure!(!media.is_empty(), "Empty AniList batch page with successor");
                }
                if chunk.iter().all(|key| records.contains_key(key)) || !has_next {
                    break;
                }
                page += count as u64;
                ensure!(page <= 100, "AniList batch pagination did not complete");
            }
            for key in chunk {
                if !records.contains_key(key) {
                    records.insert(key.clone(), self.fetch(key)?);
                }
            }
        }
        Ok(records)
    }
}
