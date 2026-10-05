use super::{Changes, Commit, Cursor, Entry, Record};
use anyhow::{Context, Result, ensure};
use postgres::{Client, IsolationLevel, Row};
use postgres_native_tls::MakeTlsConnector;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub struct Postgres {
    client: Client,
    source: String,
    domains: Vec<String>,
    scope: String,
    locked: bool,
}

impl Postgres {
    pub fn connect(url: &str, source: &str, domains: &[&str]) -> Result<Self> {
        ensure!(!source.is_empty(), "Source must not be empty");
        ensure!(
            !domains.is_empty(),
            "Select at least one acquisition domain"
        );
        let mut domains: Vec<String> = domains.iter().map(|domain| (*domain).into()).collect();
        domains.sort();
        domains.dedup();
        for domain in &domains {
            ensure!(
                !domain.is_empty()
                    && domain.bytes().all(|byte| byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'_'),
                "Invalid acquisition domain"
            );
        }
        let tls = MakeTlsConnector::new(native_tls::TlsConnector::new()?);
        let mut config: postgres::Config = url
            .parse()
            .context("Invalid ingestion PostgreSQL configuration")?;
        config.connect_timeout(std::time::Duration::from_secs(15));
        let mut client = config
            .connect(tls)
            .context("Connect to ingestion PostgreSQL")?;
        client.batch_execute("SET application_name = 'trakkin-mappings-source'; SET lock_timeout = '10s'; SET statement_timeout = '120s'; SET idle_in_transaction_session_timeout = '60s'")?;
        let mut transaction = client.transaction()?;
        transaction.query_one("SELECT pg_advisory_xact_lock(782194625103)", &[])?;
        transaction.batch_execute(include_str!("schema.sql"))?;
        transaction.execute(
            "INSERT INTO ingestion.sources(source) VALUES ($1) ON CONFLICT DO NOTHING",
            &[&source],
        )?;
        transaction.commit()?;
        Ok(Self {
            client,
            source: source.into(),
            scope: domains.join(","),
            domains,
            locked: false,
        })
    }

    fn lock(&mut self) -> Result<()> {
        if !self.locked {
            self.locked = self
                .client
                .query_one(
                    "SELECT pg_try_advisory_lock(hashtextextended('trakkin-ingestion:' || $1, 0))",
                    &[&self.source],
                )?
                .get(0);
        }
        ensure!(
            self.locked,
            "Another ingestion writer is active for {}",
            self.source
        );
        Ok(())
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn begin(&mut self) -> Result<()> {
        self.lock()?;
        ensure!(self.client.query_opt(
            "SELECT 1 FROM ingestion.sync_state WHERE source=$1 AND value->'run' IS NOT NULL
             AND value->'run' <> 'null'::jsonb AND scope<>$2 LIMIT 1",
            &[&self.source, &self.scope],
        )?.is_none(), "An unfinished sync uses another domain selection; resume or restart that selection");
        Ok(())
    }

    pub fn entries(&mut self, keys: &[String]) -> Result<BTreeMap<String, Entry>> {
        ensure!(keys.len() <= 1000, "Record lookup exceeds 1000 keys");
        Ok(self.client.query(
            "SELECT key,hash,deleted,metadata,seen_run FROM ingestion.records WHERE source=$1 AND key=ANY($2) AND domain=ANY($3)",
            &[&self.source, &keys, &self.domains],
        )?.into_iter().map(|row| (row.get(0), Entry { hash: row.get(1), deleted: row.get(2), metadata: row.get(3), seen_run: row.get(4) })).collect())
    }

    pub fn children(&mut self, parent: &str, after: &str) -> Result<Vec<String>> {
        Ok(self
            .client
            .query(
                "SELECT key FROM ingestion.records WHERE source=$1 AND metadata->>'parent'=$2
             AND key>$3 AND NOT deleted AND domain=ANY($4) ORDER BY key LIMIT 1000",
                &[&self.source, &parent, &after, &self.domains],
            )?
            .into_iter()
            .map(|row| row.get(0))
            .collect())
    }

    pub fn unseen(&mut self, run_id: &str, after: &str) -> Result<Vec<String>> {
        Ok(self.client.query(
            "SELECT key FROM ingestion.records WHERE source=$1 AND domain=ANY($2) AND NOT deleted
             AND seen_run IS DISTINCT FROM $3 AND key>$4 ORDER BY key LIMIT 1000",
            &[&self.source, &self.domains, &run_id, &after],
        )?.into_iter().map(|row| row.get(0)).collect())
    }

    pub fn state(&mut self) -> Result<Value> {
        Ok(self
            .client
            .query_opt(
                "SELECT value FROM ingestion.sync_state WHERE source=$1 AND scope=$2",
                &[&self.source, &self.scope],
            )?
            .map(|row| row.get(0))
            .unwrap_or_else(|| json!({})))
    }

    pub fn states(&mut self) -> Result<Vec<(String, Value)>> {
        Ok(self
            .client
            .query(
                "SELECT scope,value FROM ingestion.sync_state WHERE source=$1 ORDER BY scope",
                &[&self.source],
            )?
            .into_iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect())
    }

    pub fn get(&mut self, key: &str) -> Result<Option<Record>> {
        self.client.query_opt(
            "SELECT version.key,version.revision,version.hash,version.deleted,version.payload,record.metadata
             FROM ingestion.records record JOIN ingestion.record_versions version USING(source,revision,key)
             WHERE record.source=$1 AND record.key=$2 AND record.domain=ANY($3)",
            &[&self.source, &key, &self.domains],
        )?.map(record).transpose()
    }

    pub fn commit(&mut self, commit: Commit<'_>) -> Result<()> {
        ensure!(
            self.locked,
            "Acquire the source writer lock before committing"
        );
        commit.batch.validate()?;
        ensure!(
            commit.seen.len() <= 1000,
            "Observation batch exceeds 1000 records"
        );
        for key in commit.batch.updates.keys().chain(commit.seen) {
            let (domain, native) = key.split_once(':').context("Unqualified record key")?;
            ensure!(
                self.domains.iter().any(|selected| selected == domain) && !native.is_empty(),
                "Record outside acquisition scope"
            );
        }
        let mut transaction = self.client.transaction()?;
        if !commit.batch.updates.is_empty() {
            let revision: i64 = transaction.query_one(
                "UPDATE ingestion.sources SET revision=revision+1 WHERE source=$1 RETURNING revision", &[&self.source],
            )?.get(0);
            let updates = Value::Array(commit.batch.updates.iter().map(|(key, update)| json!({
                "key": key, "payload": update.content.as_ref().map(|content| content.payload()),
                "hash": update.content.as_ref().map(|content| content.hash()),
                "deleted": update.content.as_ref().map(|content| content.deleted()), "metadata": update.metadata,
            })).collect());
            transaction.execute(
                "WITH incoming AS (
                    SELECT * FROM jsonb_to_recordset($2) AS input(key text,payload text,hash text,deleted boolean,metadata jsonb)
                 ), written AS (
                    INSERT INTO ingestion.record_versions(source,revision,key,payload,hash,deleted,metadata)
                    SELECT $1,$3,input.key,COALESCE(input.payload,previous.payload),COALESCE(input.hash,previous.hash),
                           COALESCE(input.deleted,previous.deleted),input.metadata
                    FROM incoming input LEFT JOIN ingestion.records current ON current.source=$1 AND current.key=input.key
                    LEFT JOIN ingestion.record_versions previous ON previous.source=current.source AND previous.revision=current.revision AND previous.key=current.key
                    RETURNING source,key,revision,hash,deleted,metadata
                 ) INSERT INTO ingestion.records(source,key,revision,hash,deleted,metadata)
                   SELECT source,key,revision,hash,deleted,metadata FROM written
                   ON CONFLICT (source,key) DO UPDATE SET revision=EXCLUDED.revision,hash=EXCLUDED.hash,deleted=EXCLUDED.deleted,metadata=EXCLUDED.metadata",
                &[&self.source, &updates, &revision],
            )?;
        }
        transaction.execute(
            "UPDATE ingestion.records SET seen_run=$3 WHERE source=$1 AND key=ANY($2)",
            &[&self.source, &commit.seen, &commit.run_id],
        )?;
        transaction.execute(
            "INSERT INTO ingestion.sync_state(source,scope,value) VALUES ($1,$2,$3)
             ON CONFLICT (source,scope) DO UPDATE SET value=EXCLUDED.value",
            &[&self.source, &self.scope, &commit.state],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn revision(&mut self) -> Result<i64> {
        Ok(self
            .client
            .query_one(
                "SELECT revision FROM ingestion.sources WHERE source=$1",
                &[&self.source],
            )?
            .get(0))
    }

    pub fn inspect(
        &mut self,
        after: &str,
        limit: usize,
        include_deleted: bool,
    ) -> Result<Vec<Record>> {
        ensure!(
            (1..=1000).contains(&limit),
            "Limit must be between 1 and 1000"
        );
        self.client.query(
            "SELECT version.key,version.revision,version.hash,version.deleted,version.payload,record.metadata
             FROM ingestion.records record JOIN ingestion.record_versions version USING(source,revision,key)
             WHERE record.source=$1 AND record.domain=ANY($2) AND record.key>$3 AND ($4 OR NOT record.deleted)
             ORDER BY record.key LIMIT $5", &[&self.source, &self.domains, &after, &include_deleted, &(limit as i64)],
        )?.into_iter().map(record).collect()
    }

    pub fn changes(
        &mut self,
        after: &Cursor,
        through: Option<i64>,
        limit: usize,
    ) -> Result<Changes> {
        ensure!(
            (1..=1000).contains(&limit),
            "Limit must be between 1 and 1000"
        );
        let mut transaction = self
            .client
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()?;
        let head = transaction.query_one(
            "SELECT revision,history_floor FROM ingestion.sources WHERE source=$1",
            &[&self.source],
        )?;
        let head_revision: i64 = head.get(0);
        let floor: i64 = head.get(1);
        let through = through.unwrap_or(head_revision);
        ensure!(
            after.revision > floor || after.revision == floor && after.key.is_none(),
            "Change cursor expired; rebuild from current records"
        );
        ensure!(
            after.revision >= 0 && through >= after.revision && through <= head_revision,
            "Invalid change revision window"
        );
        let records: Vec<Record> = transaction.query(
            "SELECT key,revision,hash,deleted,payload,metadata FROM ingestion.record_versions
             WHERE source=$1 AND (revision>$2 OR (revision=$2 AND $3::text IS NOT NULL AND key>$3)) AND revision<=$4 AND split_part(key,':',1)=ANY($5)
             ORDER BY revision,key LIMIT $6",
            &[&self.source, &after.revision, &after.key, &through, &self.domains, &(limit as i64)],
        )?.into_iter().map(record).collect::<Result<_>>()?;
        let complete = records.len() < limit;
        let cursor = if complete {
            Cursor {
                revision: through,
                key: None,
            }
        } else {
            let record = records.last().unwrap();
            Cursor {
                revision: record.revision,
                key: Some(record.key.clone()),
            }
        };
        transaction.commit()?;
        Ok(Changes {
            records,
            cursor,
            through,
            complete,
        })
    }

    pub fn validate(&mut self) -> Result<u64> {
        let mut after = String::new();
        let mut count = 0;
        loop {
            let records = self.inspect(&after, 1000, true)?;
            if records.is_empty() {
                break;
            }
            for record in records {
                ensure!(
                    crate::content_hash(&record.payload)? == record.hash,
                    "Payload hash mismatch for {}",
                    record.key
                );
                after = record.key;
                count += 1;
            }
        }
        Ok(count)
    }
}

fn record(row: Row) -> Result<Record> {
    let payload: String = row.get(4);
    Ok(Record {
        key: row.get(0),
        revision: row.get(1),
        hash: row.get(2),
        deleted: row.get(3),
        payload: serde_json::from_str(&payload)?,
        metadata: row.get(5),
    })
}
