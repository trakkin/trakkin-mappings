use crate::projection;
use anyhow::{Context, Result, ensure};
use postgres::{Client, Row, Transaction};
use postgres_native_tls::MakeTlsConnector;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
use trakkin_mappings_language::{Record, parse_record, sha256};

mod alignment;
mod hierarchy;
mod publication;
mod retention;
mod review;
pub use publication::{PendingPublication, PublishReport};
pub use retention::RetentionReport;
pub use review::{Action, Review};

const LOCK: i64 = 0x7472616b636f7270;

pub struct Database {
    client: Client,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SyncReport {
    pub records: u64,
    pub match_pages: u64,
    pub supports: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Input {
    pub source: String,
    pub key: String,
    pub revision: i64,
    pub field: String,
    pub value: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub claim_id: String,
    pub record: String,
    pub revision: i64,
    pub supports: i64,
    pub decision: Option<String>,
    pub decision_version: Option<i64>,
}

impl Database {
    pub fn connect(url: &str) -> Result<Self> {
        let mut configuration: postgres::Config = url
            .parse()
            .context("invalid PostgreSQL connection configuration")?;
        configuration
            .application_name("trakkin-corpus")
            .connect_timeout(Duration::from_secs(15));
        let tls = native_tls::TlsConnector::new()?;
        let mut client = configuration
            .connect(MakeTlsConnector::new(tls))
            .context("cannot connect to corpus PostgreSQL")?;
        client.batch_execute("SET lock_timeout = '10s'; SET statement_timeout = '120s'; SET idle_in_transaction_session_timeout = '120s'")?;
        let mut transaction = client.transaction()?;
        transaction.query_one("SELECT pg_advisory_xact_lock($1)", &[&LOCK])?;
        ensure!(
            transaction
                .query_one(
                    "SELECT to_regclass('ingestion.record_versions') IS NOT NULL",
                    &[]
                )?
                .get::<_, bool>(0),
            "initialize source ingestion before the corpus"
        );
        transaction.batch_execute(include_str!("schema.sql"))?;
        transaction.commit()?;
        Ok(Self { client })
    }

    pub fn sync(&mut self, batch_size: usize, max_pages: Option<usize>) -> Result<SyncReport> {
        ensure!(
            (1..=1000).contains(&batch_size),
            "batch size must be between 1 and 1000"
        );
        self.refresh_engine()?;
        let mut report = SyncReport::default();
        let mut pages = 0;
        loop {
            if max_pages.is_some_and(|maximum| pages >= maximum) {
                break;
            }
            let mut transaction = self.client.transaction()?;
            transaction.query_one("SELECT pg_advisory_xact_lock($1)", &[&LOCK])?;
            transaction.execute("INSERT INTO corpus.consumers(source, revision) SELECT source, revision FROM ingestion.sources ON CONFLICT DO NOTHING", &[])?;
            let match_next: bool = transaction
                .query_one(
                    "SELECT match_next FROM corpus.scheduler WHERE singleton FOR UPDATE",
                    &[],
                )?
                .get(0);
            let source = transaction.query_opt("SELECT consumer.source, consumer.revision, consumer.after_key, consumer.bootstrap, consumer.bootstrap_key, source.revision, source.history_floor FROM corpus.consumers consumer JOIN ingestion.sources source USING(source) WHERE consumer.bootstrap OR consumer.revision < source.revision OR consumer.after_key IS NOT NULL ORDER BY consumer.scheduled_at,consumer.source LIMIT 1 FOR UPDATE OF consumer", &[])?;
            let job = transaction.query_opt("SELECT scope, after_left, after_right FROM corpus.jobs WHERE diagnostic IS NULL ORDER BY scheduled_at,scope LIMIT 1 FOR UPDATE", &[])?;
            if let Some(job) = job.filter(|_| match_next || source.is_none()) {
                let mut next = Some(job);
                let mut remaining = batch_size;
                while let Some(job) = next {
                    let scope: String = job.get(0);
                    let (supports, work) = match_page(&mut transaction, &job, remaining)?;
                    report.supports += supports;
                    transaction.execute(
                        "UPDATE corpus.jobs SET scheduled_at=clock_timestamp() WHERE scope=$1",
                        &[&scope],
                    )?;
                    remaining -= work;
                    if remaining == 0 {
                        break;
                    }
                    next = transaction.query_opt("SELECT scope, after_left, after_right FROM corpus.jobs WHERE diagnostic IS NULL ORDER BY scheduled_at,scope LIMIT 1 FOR UPDATE", &[])?;
                }
                transaction.execute(
                    "UPDATE corpus.scheduler SET match_next=false WHERE singleton",
                    &[],
                )?;
                report.match_pages += 1;
            } else if let Some(source) = source {
                let namespace: String = source.get(0);
                report.records += project_page(&mut transaction, &source, batch_size)?;
                transaction.execute(
                    "UPDATE corpus.consumers SET scheduled_at=clock_timestamp() WHERE source=$1",
                    &[&namespace],
                )?;
                transaction.execute(
                    "UPDATE corpus.scheduler SET match_next=true WHERE singleton",
                    &[],
                )?;
            } else {
                transaction.commit()?;
                break;
            }
            transaction.commit()?;
            pages += 1;
        }
        Ok(report)
    }

    pub fn candidates(&mut self, after: Option<&str>, limit: usize) -> Result<Vec<Candidate>> {
        ensure!(
            (1..=1000).contains(&limit),
            "candidate page size must be between 1 and 1000"
        );
        Ok(self.client.query("SELECT c.claim_id,c.record,c.revision,(SELECT count(*) FROM corpus.supports s WHERE s.claim_id=c.claim_id),d.action,d.version FROM corpus.candidates c LEFT JOIN corpus.decisions d USING(claim_id) WHERE ($1::text IS NULL OR c.claim_id>$1) ORDER BY c.claim_id LIMIT $2", &[&after, &(limit as i64)])?.into_iter().map(|row| Candidate { claim_id: row.get(0), record: row.get(1), revision: row.get(2), supports: row.get(3), decision: row.get(4), decision_version: row.get(5) }).collect())
    }

    pub fn show(
        &mut self,
        claim_id: &str,
        after_support: Option<&str>,
        limit: usize,
    ) -> Result<Value> {
        ensure!(
            (1..=1000).contains(&limit),
            "evidence page size must be between 1 and 1000"
        );
        let candidate = self.client.query_opt(
            "SELECT record,revision FROM corpus.candidates WHERE claim_id=$1",
            &[&claim_id],
        )?;
        let decision = self.client.query_opt(
            "SELECT record,action,version FROM corpus.decisions WHERE claim_id=$1",
            &[&claim_id],
        )?;
        ensure!(candidate.is_some() || decision.is_some(), "unknown claim");
        let mut supports = self.client.query("SELECT id,method,evidence FROM corpus.supports WHERE claim_id=$1 AND ($2::text IS NULL OR id>$2) ORDER BY id LIMIT $3", &[&claim_id,&after_support,&((limit+1) as i64)])?;
        let next_support = if supports.len() > limit {
            supports.truncate(limit);
            Some(
                supports
                    .last()
                    .context("empty support page")?
                    .get::<_, String>(0),
            )
        } else {
            None
        };
        let history = self.client.query("SELECT version,action,note FROM corpus.decision_events WHERE claim_id=$1 ORDER BY version DESC LIMIT 100", &[&claim_id])?;
        Ok(
            json!({"claim_id":claim_id, "record":candidate.as_ref().map(|row| row.get::<_,String>(0)), "candidate_revision":candidate.map(|row| row.get::<_,i64>(1)), "decision":decision.map(|row| json!({"record":row.get::<_,String>(0),"action":row.get::<_,String>(1),"version":row.get::<_,i64>(2)})), "supports":supports.into_iter().map(|row| json!({"id":row.get::<_,String>(0),"method":row.get::<_,String>(1),"evidence":row.get::<_,Value>(2)})).collect::<Vec<_>>(),"next_support":next_support, "history":history.into_iter().map(|row| json!({"version":row.get::<_,i64>(0),"action":row.get::<_,String>(1),"note":row.get::<_,String>(2)})).collect::<Vec<_>>() }),
        )
    }

    pub fn status(&mut self) -> Result<Value> {
        let sources = self.client.query("SELECT source,revision,after_key,bootstrap,bootstrap_key FROM corpus.consumers ORDER BY source", &[])?;
        let counts = self.client.query_one("SELECT (SELECT count(*) FROM corpus.records WHERE NOT deleted),(SELECT count(*) FROM corpus.candidates),(SELECT count(*) FROM corpus.supports),(SELECT count(*) FROM corpus.jobs WHERE diagnostic IS NULL),(SELECT count(*) FROM corpus.pending_publication)", &[])?;
        let diagnostics=self.client.query("SELECT scope,diagnostic FROM corpus.jobs WHERE diagnostic IS NOT NULL ORDER BY scope LIMIT 100",&[])?;
        Ok(
            json!({"records":counts.get::<_,i64>(0),"candidates":counts.get::<_,i64>(1),"supports":counts.get::<_,i64>(2),"jobs":counts.get::<_,i64>(3),"publication_pending":counts.get::<_,i64>(4),"diagnostics":diagnostics.into_iter().map(|row|json!({"scope":row.get::<_,String>(0),"message":row.get::<_,String>(1)})).collect::<Vec<_>>(),"sources":sources.into_iter().map(|row| json!({"source":row.get::<_,String>(0),"revision":row.get::<_,i64>(1),"after_key":row.get::<_,Option<String>>(2),"bootstrap":row.get::<_,bool>(3),"bootstrap_key":row.get::<_,Option<String>>(4)})).collect::<Vec<_>>() }),
        )
    }

    fn refresh_engine(&mut self) -> Result<()> {
        let mut transaction = self.client.transaction()?;
        transaction.query_one("SELECT pg_advisory_xact_lock($1)", &[&LOCK])?;
        let previous =
            transaction.query_opt("SELECT revision FROM corpus.engine WHERE singleton", &[])?;
        if previous.is_some_and(|row| row.get::<_, String>(0) != engine_revision()) {
            transaction.execute("UPDATE corpus.consumers c SET revision=s.revision,after_key=NULL,bootstrap=true,bootstrap_key=NULL FROM ingestion.sources s WHERE s.source=c.source", &[])?;
            transaction.execute("DELETE FROM corpus.jobs", &[])?;
            transaction.execute("INSERT INTO corpus.jobs(scope) SELECT 'policy:' || claim_id FROM corpus.decisions UNION ALL SELECT 'parent:' || claim_id FROM corpus.decisions", &[])?;
            transaction.execute("INSERT INTO corpus.pending_publication(claim_id) SELECT claim_id FROM corpus.decisions ON CONFLICT(claim_id) DO UPDATE SET generation=nextval('corpus.publication_generation')", &[])?;
        }
        transaction.execute("INSERT INTO corpus.engine(singleton,revision) VALUES(true,$1) ON CONFLICT(singleton) DO UPDATE SET revision=EXCLUDED.revision", &[&engine_revision()])?;
        transaction.commit()?;
        Ok(())
    }
}

fn project_page(transaction: &mut Transaction<'_>, source: &Row, batch_size: usize) -> Result<u64> {
    let namespace: String = source.get(0);
    let revision: i64 = source.get(1);
    let after_key: Option<String> = source.get(2);
    let bootstrap: bool = source.get(3);
    let bootstrap_key: Option<String> = source.get(4);
    let through: i64 = source.get(5);
    ensure!(
        revision >= source.get::<_, i64>(6),
        "corpus consumer cursor expired for {namespace}"
    );
    let limit = batch_size as i64;
    let records = if bootstrap {
        transaction.query("SELECT version.key, version.revision, version.payload, version.deleted, version.metadata FROM ingestion.records current JOIN ingestion.record_versions version USING(source, revision, key) WHERE current.source=$1 AND ($2::text IS NULL OR current.key>$2) ORDER BY current.key LIMIT $3", &[&namespace, &bootstrap_key, &limit])?
    } else {
        transaction.query("SELECT key, revision, payload, deleted, metadata FROM ingestion.record_versions WHERE source=$1 AND revision <= $4 AND (revision > $2 OR (revision=$2 AND $3::text IS NOT NULL AND key>$3)) ORDER BY revision,key LIMIT $5", &[&namespace, &revision, &after_key, &through, &limit])?
    };
    for row in &records {
        let key: String = row.get(0);
        let version: i64 = row.get(1);
        let payload: String = row.get(2);
        let deleted: bool = row.get(3);
        let metadata: Value = row.get(4);
        project_record(
            transaction,
            &namespace,
            &key,
            version,
            &serde_json::from_str(&payload)?,
            deleted,
            &metadata,
        )?;
    }
    let complete = records.len() < batch_size;
    if bootstrap {
        let key: Option<String> = records.last().map(|row| row.get(0));
        transaction.execute(
            "UPDATE corpus.consumers SET bootstrap=$2, bootstrap_key=$3 WHERE source=$1",
            &[&namespace, &!complete, &key],
        )?;
    } else {
        let (revision, key) = if complete {
            (through, None)
        } else {
            let last = records.last().context("empty source page")?;
            (last.get(1), Some(last.get::<_, String>(0)))
        };
        transaction.execute(
            "UPDATE corpus.consumers SET revision=$2, after_key=$3 WHERE source=$1",
            &[&namespace, &revision, &key],
        )?;
    }
    Ok(records.len() as u64)
}

fn match_page(
    transaction: &mut Transaction<'_>,
    job: &Row,
    batch_size: usize,
) -> Result<(u64, usize)> {
    let scope: String = job.get(0);
    let left: String = job.get(1);
    let right: String = job.get(2);
    if let Some(claim_id) = scope.strip_prefix("policy:") {
        review::refresh_footprint(transaction, claim_id)?;
        transaction.execute("DELETE FROM corpus.jobs WHERE scope=$1", &[&scope])?;
        return Ok((0, 1));
    }
    if let Some(claim_id) = scope.strip_prefix("parent:") {
        let result = {
            let mut page = transaction.transaction()?;
            match hierarchy::page(&mut page, claim_id, &left, batch_size) {
                Ok(result) => {
                    page.commit()?;
                    Ok(result)
                }
                Err(error) => {
                    page.rollback()?;
                    Err(error)
                }
            }
        };
        return match result {
            Ok((next, supports)) => {
                if let Some(next) = next {
                    transaction.execute(
                        "UPDATE corpus.jobs SET after_left=$2 WHERE scope=$1",
                        &[&scope, &next],
                    )?;
                } else {
                    transaction.execute("DELETE FROM corpus.jobs WHERE scope=$1", &[&scope])?;
                }
                Ok((supports, batch_size))
            }
            Err(error) => {
                transaction.execute(
                    "UPDATE corpus.jobs SET diagnostic=$2 WHERE scope=$1",
                    &[&scope, &error.to_string()],
                )?;
                Ok((0, batch_size))
            }
        };
    }
    let count: i64 = transaction
        .query_one(
            "SELECT count(*) FROM (SELECT 1 FROM corpus.blocks WHERE scope=$1 LIMIT 257) members",
            &[&scope],
        )?
        .get(0);
    if scope.starts_with("title:") && count > 256 {
        let evidence = json!([{"field":"block","value":scope}]);
        let removed = transaction.query("DELETE FROM corpus.supports WHERE id IN (SELECT id FROM corpus.supports WHERE method='title-year' AND evidence @> $1 ORDER BY id LIMIT $2) RETURNING claim_id", &[&evidence, &(batch_size as i64)])?;
        let claims: Vec<String> = removed.iter().map(|row| row.get(0)).collect();
        transaction.execute(
            "UPDATE corpus.candidates SET revision=revision+1 WHERE claim_id=ANY($1)",
            &[&claims],
        )?;
        if removed.len() < batch_size {
            transaction.execute("UPDATE corpus.jobs SET diagnostic='ambiguous title/year block exceeds 256 entities; no automatic pair expansion' WHERE scope=$1", &[&scope])?;
        }
        return Ok((0, removed.len().max(1)));
    }
    let rows = transaction.query("SELECT a.uri, b.uri, ar.source, ar.key, ar.revision, ar.kind, br.source, br.key, br.revision, br.kind FROM corpus.blocks a JOIN corpus.blocks b ON b.scope=a.scope AND b.uri>a.uri JOIN corpus.records ar ON ar.uri=a.uri JOIN corpus.records br ON br.uri=b.uri WHERE a.scope=$1 AND ar.source<>br.source AND ar.engine=$5 AND br.engine=$5 AND (a.uri,b.uri)>($2,$3) ORDER BY a.uri,b.uri LIMIT $4", &[&scope, &left, &right, &(batch_size as i64), &engine_revision()])?;
    let mut supports = 0;
    for row in &rows {
        let left_kind: String = row.get(5);
        let right_kind: String = row.get(9);
        if left_kind != right_kind && left_kind != "unit" && right_kind != "unit" {
            continue;
        }
        let inputs = vec![
            Input {
                source: row.get(2),
                key: row.get(3),
                revision: row.get(4),
                field: "block".into(),
                value: Value::String(scope.clone()),
            },
            Input {
                source: row.get(6),
                key: row.get(7),
                revision: row.get(8),
                field: "block".into(),
                value: Value::String(scope.clone()),
            },
        ];
        let record = parse_record(&format!(
            "{} <=> {}\n",
            row.get::<_, String>(0),
            row.get::<_, String>(1)
        ))?;
        if add_support(
            transaction,
            &record,
            if scope.starts_with("id:") {
                "shared-identifier"
            } else {
                "title-year"
            },
            &inputs,
        )? {
            supports += 1;
        }
    }
    if rows.len() < batch_size {
        transaction.execute("DELETE FROM corpus.jobs WHERE scope=$1", &[&scope])?;
    } else {
        let last = rows.last().context("empty match page")?;
        transaction.execute(
            "UPDATE corpus.jobs SET after_left=$2,after_right=$3 WHERE scope=$1",
            &[&scope, &last.get::<_, String>(0), &last.get::<_, String>(1)],
        )?;
    }
    Ok((supports, rows.len().max(1)))
}

fn project_record(
    transaction: &mut Transaction<'_>,
    source: &str,
    key: &str,
    revision: i64,
    payload: &Value,
    deleted: bool,
    metadata: &Value,
) -> Result<()> {
    let previous = transaction.query_opt(
        "SELECT revision,uri,parent,engine FROM corpus.records WHERE source=$1 AND key=$2",
        &[&source, &key],
    )?;
    if previous.as_ref().is_some_and(|row| {
        row.get::<_, i64>(0) >= revision && row.get::<_, String>(3) == engine_revision()
    }) {
        return Ok(());
    }
    let fact = projection::project(source, key, payload)?;
    let old_scopes = transaction.query(
        "DELETE FROM corpus.blocks WHERE uri=$1 RETURNING scope",
        &[&fact.uri],
    )?;
    let affected = transaction.query("DELETE FROM corpus.supports s WHERE EXISTS(SELECT 1 FROM corpus.support_inputs i WHERE i.support_id=s.id AND i.source=$1 AND i.key=$2) RETURNING claim_id", &[&source, &key])?;
    for row in affected {
        transaction.execute(
            "UPDATE corpus.candidates SET revision=revision+1 WHERE claim_id=$1",
            &[&row.get::<_, String>(0)],
        )?;
    }
    let parent = metadata["parent"].as_str();
    transaction.execute("INSERT INTO corpus.records(source,key,revision,uri,parent,deleted,kind,engine) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(source,key) DO UPDATE SET revision=EXCLUDED.revision,parent=EXCLUDED.parent,deleted=EXCLUDED.deleted,kind=EXCLUDED.kind,engine=EXCLUDED.engine", &[&source, &key, &revision, &fact.uri, &parent, &deleted, &fact.kind, &engine_revision()])?;
    let mut scopes: BTreeSet<String> = old_scopes.into_iter().map(|row| row.get(0)).collect();
    if !deleted {
        for scope in fact.blocks() {
            transaction.execute(
                "INSERT INTO corpus.blocks(scope,uri) VALUES($1,$2) ON CONFLICT DO NOTHING",
                &[&scope, &fact.uri],
            )?;
            scopes.insert(scope);
        }
        for (field, target) in &fact.identifiers {
            let record = parse_record(&format!("{} <=> {target}\n", fact.uri))?;
            let input = Input {
                source: source.into(),
                key: key.into(),
                revision,
                field: field.clone(),
                value: payload.pointer(field).cloned().unwrap_or(Value::Null),
            };
            add_support(transaction, &record, "explicit-identifier", &[input])?;
        }
    }
    for scope in scopes {
        transaction.execute("INSERT INTO corpus.jobs(scope) VALUES($1) ON CONFLICT(scope) DO UPDATE SET after_left='',after_right='',diagnostic=NULL", &[&scope])?;
    }
    let mut endpoints = vec![fact.uri];
    for parent in [
        parent,
        previous
            .as_ref()
            .and_then(|row| row.get::<_, Option<String>>(2))
            .as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        endpoints.push(projection::reference(source, parent)?);
    }
    transaction.execute("INSERT INTO corpus.pending_publication(claim_id) SELECT DISTINCT claim_id FROM corpus.decision_endpoints WHERE uri=ANY($1) ON CONFLICT(claim_id) DO UPDATE SET generation=nextval('corpus.publication_generation')", &[&endpoints])?;
    transaction.execute("INSERT INTO corpus.jobs(scope) SELECT DISTINCT 'policy:' || claim_id FROM corpus.decision_endpoints WHERE uri=ANY($1) ON CONFLICT(scope) DO UPDATE SET diagnostic=NULL", &[&endpoints])?;
    hierarchy::invalidate(transaction, &endpoints)?;
    Ok(())
}

fn add_support(
    transaction: &mut Transaction<'_>,
    record: &Record,
    method: &str,
    inputs: &[Input],
) -> Result<bool> {
    let claim_id = record.statement.claim_id().to_string();
    let id = support_id(record, method, inputs)?;
    transaction.execute(
        "INSERT INTO corpus.candidates(claim_id,record) VALUES($1,$2) ON CONFLICT DO NOTHING",
        &[&claim_id, &record.canonical()],
    )?;
    let inserted = transaction.execute("INSERT INTO corpus.supports(id,claim_id,method,evidence,engine) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING", &[&id, &claim_id, &method, &serde_json::to_value(inputs)?,&engine_revision()])? > 0;
    if inserted {
        for input in inputs {
            transaction.execute("INSERT INTO corpus.support_inputs(support_id,source,revision,key) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING", &[&id, &input.source, &input.revision, &input.key])?;
        }
        transaction.execute(
            "UPDATE corpus.candidates SET revision=revision+1 WHERE claim_id=$1",
            &[&claim_id],
        )?;
    }
    Ok(inserted)
}

fn support_id(record: &Record, method: &str, inputs: &[Input]) -> Result<String> {
    Ok(sha256(&serde_json::to_vec(&(
        engine_revision(),
        record.statement.claim_id().to_string(),
        method,
        inputs,
    ))?))
}

fn engine_revision() -> &'static str {
    static REVISION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    REVISION.get_or_init(|| {
        sha256(
            &[
                include_bytes!("pipeline.rs").as_slice(),
                include_bytes!("pipeline/review.rs"),
                include_bytes!("pipeline/hierarchy.rs"),
                include_bytes!("pipeline/alignment.rs"),
                include_bytes!("projection.rs"),
                include_bytes!("policy.rs"),
                include_bytes!("publication.rs"),
                include_bytes!("adapters/common.rs"),
                include_bytes!("adapters/catalog.rs"),
                include_bytes!("adapters/anilist.rs"),
                include_bytes!("adapters/tmdb.rs"),
                include_bytes!("adapters/tvdb.rs"),
                trakkin_mappings_language::semantic_revision().as_bytes(),
            ]
            .concat(),
        )
    })
}
