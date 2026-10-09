use super::*;
use crate::{Bindings, ConflictPolicy, Resolution};
use trakkin_mappings_language::{Resolver, evaluate};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Accepted,
    Rejected,
    Retracted,
}
impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Retracted => "retracted",
        }
    }
}

pub struct Review {
    pub record: String,
    pub action: Action,
    pub note: String,
    pub expected_candidate: Option<i64>,
    pub expected_decision: Option<i64>,
    pub replaces: Option<(String, i64)>,
}

impl Database {
    pub fn set_policy(&mut self, policy: &ConflictPolicy) -> Result<()> {
        let document = serde_json::to_value(policy)?;
        ConflictPolicy::parse(&document.to_string())?;
        let mut transaction = self.client.transaction()?;
        transaction.query_one("SELECT pg_advisory_xact_lock($1)", &[&LOCK])?;
        let changed = transaction.execute("UPDATE corpus.policy SET document=$1,generation=generation+1 WHERE singleton AND document<>$1", &[&document])?;
        if changed > 0 {
            transaction.execute("INSERT INTO corpus.jobs(scope) SELECT 'policy:' || claim_id FROM corpus.decisions ON CONFLICT(scope) DO UPDATE SET diagnostic=NULL", &[])?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn review(&mut self, command: Review) -> Result<i64> {
        ensure!(
            !command.note.trim().is_empty() && command.note.len() <= 8192,
            "review requires a note of at most 8192 bytes"
        );
        let mut record = parse_record(&command.record)?;
        ensure!(
            record.canonical().len() <= 2 * 1024 * 1024,
            "mapping record exceeds 2 MiB"
        );
        ensure!(
            !record
                .metadata
                .iter()
                .any(|line| line.starts_with("#@decision ") || line.starts_with("#@resolve ")),
            "decision and resolution annotations are maintained by publication"
        );
        let claim_id = record.statement.claim_id().to_string();
        let mut transaction = self.client.transaction()?;
        transaction.query_one("SELECT pg_advisory_xact_lock($1)", &[&LOCK])?;
        let existing = transaction
            .query_opt(
                "SELECT version FROM corpus.decisions WHERE claim_id=$1",
                &[&claim_id],
            )?
            .map(|row| row.get::<_, i64>(0));
        ensure!(
            existing == command.expected_decision,
            "decision changed; supply its current version"
        );
        if let Some(expected) = command.expected_candidate {
            let revision = transaction
                .query_opt(
                    "SELECT revision FROM corpus.candidates WHERE claim_id=$1",
                    &[&claim_id],
                )?
                .map(|row| row.get::<_, i64>(0));
            ensure!(
                revision == Some(expected),
                "candidate evidence changed; review it again"
            );
            if command.action == Action::Accepted {
                let valid: bool = transaction.query_one("SELECT EXISTS(SELECT 1 FROM corpus.supports s WHERE s.claim_id=$1 AND s.engine=$2 AND NOT EXISTS(SELECT 1 FROM corpus.support_inputs i LEFT JOIN ingestion.records r ON r.source=i.source AND r.key=i.key WHERE i.support_id=s.id AND (r.revision IS DISTINCT FROM i.revision OR r.deleted)))", &[&claim_id,&engine_revision()])?.get(0);
                ensure!(valid, "candidate has no current supporting evidence");
            }
        }
        let mut native_inputs = Vec::new();
        if command.action == Action::Accepted {
            ensure!(transaction.query_opt("SELECT 1 FROM corpus.jobs WHERE scope>='policy:' AND scope<'policy;' LIMIT 1", &[])?.is_none(), "run corpus sync to refresh policy dependencies before accepting claims");
            let (bindings, inputs) = bindings(&mut transaction, &record)?;
            native_inputs = inputs;
            let evaluation = evaluate(&record.statement, &bindings)?;
            let policy = policy(&mut transaction)?;
            for ((origin, target_source), targets) in policy.targets(&evaluation) {
                ensure!(targets.len() <= 1, "claim violates directional exclusivity");
                for target in targets {
                    let conflicting: bool = transaction.query_one("SELECT EXISTS(SELECT 1 FROM corpus.footprints WHERE origin=$1 AND target_source=$2 AND target<>$3 AND claim_id<>$4 AND ($5::text IS NULL OR claim_id<>$5))", &[&origin,&target_source,&target,&claim_id,&command.replaces.as_ref().map(|(id,_)| id.as_str())])?.get(0);
                    ensure!(
                        !conflicting,
                        "claim conflicts with an accepted mapping under the explicit directional policy"
                    );
                }
            }
        }
        if let Some((replaces, expected)) = &command.replaces {
            ensure!(
                *replaces != claim_id && command.action == Action::Accepted,
                "correction requires a different accepted replacement claim"
            );
            let previous = transaction
                .query_opt(
                    "SELECT record,version FROM corpus.decisions WHERE claim_id=$1",
                    &[replaces],
                )?
                .context("correction target does not exist")?;
            ensure!(
                previous.get::<_, i64>(1) == *expected,
                "correction target changed"
            );
            save_decision(
                &mut transaction,
                &parse_record(&previous.get::<_, String>(0))?,
                Action::Rejected,
                &command.note,
            )?;
        }
        record.metadata.push(format!(
            "#@note {}",
            command.note.trim().replace(['\r', '\n'], " ")
        ));
        ensure!(
            record.canonical().len() <= 2 * 1024 * 1024,
            "mapping record exceeds 2 MiB after adding its review note"
        );
        let version = save_decision(&mut transaction, &record, command.action, &command.note)?;
        for input in native_inputs {
            transaction.execute("INSERT INTO corpus.decision_inputs(version,source,revision,key,evidence) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING",&[&version,&input.source,&input.revision,&input.key,&json!({"purpose":"selector-validation"})])?;
        }
        if command.action == Action::Accepted && command.expected_candidate.is_some() {
            transaction.execute("INSERT INTO corpus.decision_inputs(version,source,revision,key,evidence) SELECT $2,i.source,i.revision,i.key,jsonb_agg(s.evidence ORDER BY s.id) FROM corpus.supports s JOIN corpus.support_inputs i ON i.support_id=s.id WHERE s.claim_id=$1 GROUP BY i.source,i.revision,i.key ON CONFLICT DO NOTHING", &[&claim_id,&version])?;
        }
        refresh_footprint(&mut transaction, &claim_id)?;
        transaction.commit()?;
        Ok(version)
    }
}

fn save_decision(
    transaction: &mut Transaction<'_>,
    record: &Record,
    action: Action,
    note: &str,
) -> Result<i64> {
    let claim_id = record.statement.claim_id().to_string();
    let canonical = record.canonical();
    let version: i64 = transaction.query_one("INSERT INTO corpus.decision_events(claim_id,record,action,note) VALUES($1,$2,$3,$4) RETURNING version", &[&claim_id,&canonical,&action.name(),&note])?.get(0);
    let withdrawn = transaction.query("DELETE FROM corpus.supports s USING corpus.support_decisions dependency,corpus.decisions d WHERE dependency.support_id=s.id AND dependency.version=d.version AND d.claim_id=$1 RETURNING s.claim_id", &[&claim_id])?;
    for row in withdrawn {
        transaction.execute(
            "UPDATE corpus.candidates SET revision=revision+1 WHERE claim_id=$1",
            &[&row.get::<_, String>(0)],
        )?;
    }
    transaction.execute("INSERT INTO corpus.decisions(claim_id,version,record,action) VALUES($1,$2,$3,$4) ON CONFLICT(claim_id) DO UPDATE SET version=EXCLUDED.version,record=EXCLUDED.record,action=EXCLUDED.action", &[&claim_id,&version,&canonical,&action.name()])?;
    transaction.execute(
        "DELETE FROM corpus.decision_endpoints WHERE claim_id=$1",
        &[&claim_id],
    )?;
    for selection in record
        .statement
        .left
        .selections()
        .into_iter()
        .chain(record.statement.right.selections())
    {
        transaction.execute("INSERT INTO corpus.decision_endpoints(claim_id,uri) VALUES($1,$2) ON CONFLICT DO NOTHING", &[&claim_id,&selection.reference])?;
    }
    transaction.execute("INSERT INTO corpus.pending_publication(claim_id) VALUES($1) ON CONFLICT(claim_id) DO UPDATE SET generation=nextval('corpus.publication_generation')", &[&claim_id])?;
    transaction.execute(
        "DELETE FROM corpus.footprints WHERE claim_id=$1",
        &[&claim_id],
    )?;
    transaction.execute("INSERT INTO corpus.jobs(scope) VALUES($1) ON CONFLICT(scope) DO UPDATE SET after_left='',after_right='',diagnostic=NULL", &[&format!("parent:{claim_id}")])?;
    Ok(version)
}

pub(super) fn policy(transaction: &mut Transaction<'_>) -> Result<ConflictPolicy> {
    let document: Value = transaction
        .query_one("SELECT document FROM corpus.policy WHERE singleton", &[])?
        .get(0);
    ConflictPolicy::parse(&document.to_string())
}

pub(super) fn refresh_footprint(transaction: &mut Transaction<'_>, claim_id: &str) -> Result<()> {
    transaction.execute(
        "DELETE FROM corpus.footprints WHERE claim_id=$1",
        &[&claim_id],
    )?;
    let decision = transaction.query_opt(
        "SELECT record FROM corpus.decisions WHERE claim_id=$1 AND action='accepted'",
        &[&claim_id],
    )?;
    if let Some(decision) = decision {
        let record = parse_record(&decision.get::<_, String>(0))?;
        let (bindings, _) = bindings(transaction, &record)?;
        if let Ok(evaluation) = evaluate(&record.statement, &bindings) {
            for ((origin, target_source), targets) in policy(transaction)?.targets(&evaluation) {
                for target in targets {
                    transaction.execute("INSERT INTO corpus.footprints(claim_id,origin,target_source,target) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING", &[&claim_id,&origin,&target_source,&target])?;
                }
            }
        }
    }
    Ok(())
}

pub(super) fn scope_records(
    transaction: &mut Transaction<'_>,
    reference: &str,
) -> Result<(String, BTreeMap<String, Value>, Vec<Input>)> {
    let (source, opaque) = reference
        .split_once("://")
        .context("invalid native reference")?;
    let current=transaction.query_opt("SELECT NOT c.bootstrap AND c.after_key IS NULL AND c.revision=s.revision FROM corpus.consumers c JOIN ingestion.sources s USING(source) WHERE source=$1",&[&source])?.is_some_and(|row|row.get::<_,bool>(0));
    ensure!(
        current,
        "run corpus sync before resolving changed native catalog metadata for {source}"
    );
    let (kind, id) = opaque
        .split_once('/')
        .context("native reference has no kind")?;
    let key = format!("{kind}:{id}");
    let rows = transaction.query("SELECT projected.key,projected.revision,version.payload FROM corpus.records projected JOIN ingestion.record_versions version USING(source,revision,key) WHERE projected.source=$1 AND (projected.key=$2 OR projected.parent=$2) AND NOT projected.deleted ORDER BY projected.key LIMIT 1001", &[&source,&key])?;
    ensure!(
        rows.len() <= 1000,
        "native selector scope exceeds 1000 records; split the mapping into explicit scopes"
    );
    let mut records = BTreeMap::new();
    let mut inputs = Vec::new();
    for row in rows {
        let key: String = row.get(0);
        let payload: Value = serde_json::from_str(&row.get::<_, String>(2))?;
        inputs.push(Input {
            source: source.into(),
            key: key.clone(),
            revision: row.get(1),
            field: String::new(),
            value: Value::Null,
        });
        records.insert(key, payload);
    }
    Ok((source.into(), records, inputs))
}

pub(super) fn bindings(
    transaction: &mut Transaction<'_>,
    record: &Record,
) -> Result<(Bindings, Vec<Input>)> {
    let mut bindings = Bindings::default();
    let mut inputs = BTreeMap::new();
    for selection in record
        .statement
        .left
        .selections()
        .into_iter()
        .chain(record.statement.right.selections())
        .filter(|selection| selection.selector.is_some())
    {
        let resolved = (|| {
            let (source, records, dependencies) = scope_records(transaction, &selection.reference)?;
            for input in dependencies {
                inputs.insert((input.source.clone(), input.key.clone()), input);
            }
            Ok::<_, anyhow::Error>(projection::catalog(&source, &records)?.resolve(selection)?)
        })();
        let resolution = match resolved {
            Ok(resolved) => Resolution {
                units: resolved.units,
                ordered: resolved.ordered,
                coordinates: resolved.coordinates,
                error: None,
            },
            Err(error) => Resolution {
                units: Vec::new(),
                ordered: false,
                coordinates: None,
                error: Some(error.to_string()),
            },
        };
        bindings.0.insert(selection.selection_key(), resolution);
    }
    Ok((bindings, inputs.into_values().collect()))
}
