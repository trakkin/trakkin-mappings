use super::catalogue::Reconciliation;
use super::{Http, array, id};
use crate::{Change, Operation, Page, Provider, Resume};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::Duration;

const MEDIA_FIELDS: &str = "id idMal title{romaji english native} type format status description startDate{year month day} endDate{year month day} season seasonYear episodes duration chapters volumes countryOfOrigin source genres synonyms isAdult siteUrl relations{edges{id relationType(version:3) node{id type}}}";

const IDS_PER_BATCH: usize = 50;
const CATALOGUE_BATCHES: usize = 160;
const PAYLOAD_BATCHES: usize = 11;
const AIRING_PAGES: usize = 83;
const MAX_PAGES: u64 = 100;
const AIRING_OVERLAP: i64 = 14 * 86400;

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CatalogueCursor {
    #[serde(default)]
    after: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    through: Option<u64>,
}

struct MediaBatch {
    records: BTreeMap<u64, Value>,
    latest: Option<u64>,
}

fn media_id(key: &str) -> Result<u64> {
    let media_id = key
        .strip_prefix("media:")
        .context("Invalid AniList key")?
        .parse::<u64>()?;
    ensure!(
        media_id > 0 && media_id <= i32::MAX as u64,
        "Invalid AniList ID"
    );
    ensure!(key == format!("media:{media_id}"), "Invalid AniList key");
    Ok(media_id)
}

fn airing_query(count: usize) -> String {
    let mut arguments = vec!["$after:Int!".to_owned(), "$before:Int!".to_owned()];
    let mut fields = Vec::new();
    for offset in 0..count {
        arguments.push(format!("$page{offset}:Int!"));
        fields.push(format!("page{offset}:Page(page:$page{offset},perPage:50){{pageInfo{{hasNextPage}} airingSchedules(airingAt_greater:$after,airingAt_lesser:$before,sort:ID){{mediaId airingAt}}}}"));
    }
    format!("query({}){{{}}}", arguments.join(","), fields.join(" "))
}

pub struct AniList {
    http: Http,
    endpoint: String,
    after: u64,
    existing: HashSet<u64>,
    reconciliation: Reconciliation,
}

impl AniList {
    pub const DOMAINS: &[&str] = &["media"];

    pub fn new() -> Result<Self> {
        Self::with_endpoint("https://graphql.anilist.co")
    }
    pub fn with_endpoint(endpoint: &str) -> Result<Self> {
        Self::with_endpoint_and_interval(endpoint, Duration::from_secs(2))
    }
    pub fn with_endpoint_and_interval(endpoint: &str, interval: Duration) -> Result<Self> {
        Ok(Self {
            http: Http::new(interval)?,
            endpoint: endpoint.into(),
            after: 0,
            existing: HashSet::new(),
            reconciliation: Reconciliation::default(),
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
    fn media_batches(&mut self, ids: &[u64], fields: &str, latest: bool) -> Result<MediaBatch> {
        let mut arguments = Vec::new();
        let mut selections = Vec::new();
        let mut variables = json!({});
        if latest {
            selections.push("latest:Media(sort:ID_DESC){id}".to_owned());
        }
        for (offset, chunk) in ids.chunks(IDS_PER_BATCH).enumerate() {
            arguments.push(format!("$ids{offset}:[Int!]!"));
            variables[format!("ids{offset}")] = json!(chunk);
            selections.push(format!("batch{offset}:Page(page:1,perPage:50){{media(id_in:$ids{offset},sort:ID){{{fields}}}}}"));
        }
        let arguments = if arguments.is_empty() {
            String::new()
        } else {
            format!("({})", arguments.join(","))
        };
        let response = self.query(
            &format!("query{arguments}{{{}}}", selections.join(" ")),
            variables,
            false,
        )?;
        let mut records = BTreeMap::new();
        for (offset, chunk) in ids.chunks(IDS_PER_BATCH).enumerate() {
            for record in array(&response[format!("batch{offset}")], "media")? {
                let media_id = id(record, "id")?;
                ensure!(
                    chunk.contains(&media_id),
                    "AniList returned an unrequested record"
                );
                ensure!(
                    records.insert(media_id, record.clone()).is_none(),
                    "Duplicate AniList media ID {media_id}"
                );
            }
        }
        let latest = if latest {
            let record = response
                .get("latest")
                .context("Missing AniList latest ID")?;
            Some(if record.is_null() {
                0
            } else {
                id(record, "id")?
            })
        } else {
            None
        };
        Ok(MediaBatch { records, latest })
    }

    fn catalogue(
        &mut self,
        cursor: &CatalogueCursor,
        missing_only: bool,
    ) -> Result<(BTreeSet<u64>, Option<CatalogueCursor>)> {
        ensure!(
            cursor.after <= i32::MAX as u64
                && cursor
                    .through
                    .is_none_or(|through| through > cursor.after && through <= i32::MAX as u64),
            "Invalid AniList catalogue cursor"
        );
        let limit = cursor.through.unwrap_or(i32::MAX as u64);
        let capacity = CATALOGUE_BATCHES * IDS_PER_BATCH;
        let ids: Vec<_> = (cursor.after + 1..=limit)
            .filter(|media_id| !missing_only || !self.existing.contains(media_id))
            .take(capacity)
            .collect();
        let end = if ids.len() == capacity {
            *ids.last().unwrap()
        } else {
            limit
        };
        if ids.is_empty() && cursor.through.is_some() {
            self.after = self.after.max(end);
            return Ok((BTreeSet::new(), None));
        }
        let response = self.media_batches(&ids, "id", cursor.through.is_none())?;
        let through = cursor
            .through
            .or(response.latest)
            .context("Missing AniList catalogue boundary")?;
        let after = end.min(through).max(cursor.after);
        self.after = self.after.max(after);
        let ids = response
            .records
            .into_keys()
            .filter(|media_id| *media_id <= through)
            .collect();
        let next = (after < through).then_some(CatalogueCursor {
            after,
            through: Some(through),
        });
        Ok((ids, next))
    }

    fn airing_ids(&mut self, start: i64, end: i64) -> Result<BTreeSet<u64>> {
        let mut ids = BTreeSet::new();
        let mut windows = vec![(start, end)];
        while let Some((start, end)) = windows.pop() {
            let mut page = 1;
            loop {
                let count = AIRING_PAGES.min((MAX_PAGES + 1 - page) as usize);
                let mut variables = json!({"after":start-1,"before":end+1});
                for offset in 0..count {
                    variables[format!("page{offset}")] = json!(page + offset as u64);
                }
                let response = self.query(&airing_query(count), variables, false)?;
                let mut has_next = false;
                for offset in 0..count {
                    let result = &response[format!("page{offset}")];
                    let schedules = array(result, "airingSchedules")?;
                    has_next = result["pageInfo"]["hasNextPage"]
                        .as_bool()
                        .context("Missing AniList airing pageInfo")?;
                    ensure!(
                        !schedules.is_empty() || !has_next,
                        "Empty AniList airing page with successor"
                    );
                    for schedule in schedules {
                        let aired = schedule["airingAt"]
                            .as_i64()
                            .context("Missing AniList airingAt")?;
                        ensure!(
                            aired >= start && aired <= end,
                            "AniList airing outside requested window"
                        );
                        let media_id = id(schedule, "mediaId")?;
                        ensure!(
                            media_id <= i32::MAX as u64,
                            "Invalid AniList airing media ID"
                        );
                        ids.insert(media_id);
                    }
                    if !has_next {
                        break;
                    }
                }
                if !has_next {
                    break;
                }
                page += count as u64;
                if page > MAX_PAGES {
                    ensure!(
                        start < end,
                        "AniList airing events exceed pagination depth within one second"
                    );
                    let middle = start + (end - start) / 2;
                    windows.push((middle + 1, end));
                    windows.push((start, middle));
                    break;
                }
            }
        }
        Ok(ids)
    }

    fn catalogue_page(&mut self, cursor: &Value, missing_only: bool) -> Result<Page> {
        let cursor = if cursor.is_null() {
            CatalogueCursor::default()
        } else {
            serde_json::from_value(cursor.clone()).context("Invalid AniList catalogue cursor")?
        };
        let (ids, next) = self.catalogue(&cursor, missing_only)?;
        Ok(Page {
            changes: ids
                .into_iter()
                .map(|media_id| Change::Dirty(format!("media:{media_id}")))
                .collect(),
            next: next.map(serde_json::to_value).transpose()?,
        })
    }
    fn changes(&mut self, watermark: i64, cursor: &Value, until: i64) -> Result<Page> {
        ensure!(
            cursor.is_null(),
            "AniList sync discovery does not use page cursors"
        );
        ensure!(
            watermark >= 0 && until >= watermark && until < i32::MAX as i64,
            "Invalid AniList sync time window"
        );
        let mut cursor = CatalogueCursor {
            after: self.after,
            through: None,
        };
        let mut ids = BTreeSet::new();
        loop {
            let (discovered, next) = self.catalogue(&cursor, false)?;
            ids.extend(discovered);
            let Some(next) = next else { break };
            cursor = next;
        }
        ids.extend(self.airing_ids(watermark.saturating_sub(AIRING_OVERLAP).max(0), until)?);
        Ok(Page {
            changes: ids
                .into_iter()
                .map(|media_id| Change::Dirty(format!("media:{media_id}")))
                .collect(),
            next: None,
        })
    }
}

impl Provider for AniList {
    fn source(&self) -> &'static str {
        "co.anilist"
    }
    fn resume(&self, operation: Operation) -> Resume {
        if operation == Operation::Bootstrap {
            Resume::Cursor
        } else {
            Resume::Restart
        }
    }
    fn restore(
        &mut self,
        operation: Operation,
        index: &BTreeMap<String, crate::storage::Entry>,
        checkpoint: &Value,
    ) -> Result<()> {
        let checkpoint: CatalogueCursor = if checkpoint.is_null() {
            CatalogueCursor::default()
        } else {
            serde_json::from_value(checkpoint.clone()).context("Invalid AniList checkpoint")?
        };
        ensure!(
            checkpoint.after <= i32::MAX as u64 && checkpoint.through.is_none(),
            "Invalid AniList checkpoint"
        );
        self.after = checkpoint.after;
        self.existing = if operation == Operation::Bootstrap {
            index
                .iter()
                .filter(|(key, entry)| !entry.deleted && key.starts_with("media:"))
                .map(|(key, _)| media_id(key))
                .collect::<Result<_>>()?
        } else {
            HashSet::new()
        };
        self.reconciliation =
            Reconciliation::restore(operation, index, |domain| Self::DOMAINS.contains(&domain));
        Ok(())
    }
    fn checkpoint(&self) -> Value {
        json!({"after":self.after})
    }
    fn discover(
        &mut self,
        operation: Operation,
        watermark: Option<i64>,
        cursor: &Value,
        until: i64,
    ) -> Result<Page> {
        if operation == Operation::Sync {
            return self.changes(
                watermark.context("Missing AniList watermark")?,
                cursor,
                until,
            );
        }
        if operation == Operation::Reconcile
            && let Some(page) = self.reconciliation.absences(cursor, Change::Dirty)?
        {
            return Ok(page);
        }
        let mut page = self.catalogue_page(cursor, operation == Operation::Bootstrap)?;
        if operation == Operation::Reconcile {
            self.reconciliation.observe(&mut page)?;
        }
        Ok(page)
    }
    fn fetch(&mut self, key: &str) -> Result<Option<Value>> {
        let media_id = media_id(key)?;
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
    fn fetch_batch_size(&self) -> usize {
        PAYLOAD_BATCHES * IDS_PER_BATCH
    }
    fn fetch_many(&mut self, keys: &[String]) -> Result<BTreeMap<String, Option<Value>>> {
        let mut records = BTreeMap::new();
        let ids = keys
            .iter()
            .map(|key| media_id(key))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            ids.iter().copied().collect::<BTreeSet<_>>().len() == ids.len(),
            "Duplicate AniList fetch key"
        );
        for chunk in ids.chunks(PAYLOAD_BATCHES * IDS_PER_BATCH) {
            let mut fetched = self.media_batches(chunk, MEDIA_FIELDS, false)?.records;
            for media_id in chunk {
                let key = format!("media:{media_id}");
                let record = if let Some(record) = fetched.remove(media_id) {
                    Some(record)
                } else {
                    self.fetch(&key)?
                };
                records.insert(key, record);
            }
        }
        Ok(records)
    }
}
