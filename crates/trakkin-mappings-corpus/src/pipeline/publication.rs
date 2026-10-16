use super::*;
use crate::{
    Artifact, POLICY, claim_path,
    git::{History, Prepared},
};
use tokio::runtime::Runtime;

#[derive(Clone, Debug, Serialize)]
pub struct PendingPublication {
    pub id: i64,
    pub prepared: Prepared,
    pub remote_base: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PublishReport {
    pub batches: u64,
    pub commit: Option<String>,
}

#[derive(Default)]
struct PublicationBatch {
    changes: BTreeMap<String, Option<String>>,
    generations: BTreeMap<String, i64>,
    inputs: BTreeMap<(String, String), Input>,
    bytes: usize,
}

impl PublicationBatch {
    fn push(
        &mut self,
        claim_id: String,
        generation: i64,
        path: String,
        text: Option<String>,
        inputs: Vec<Input>,
    ) -> bool {
        let bytes = text.as_ref().map_or(0, String::len);
        if !self.changes.is_empty() && self.bytes + bytes > 6 * 1024 * 1024 {
            return false;
        }
        self.bytes += bytes;
        self.changes.insert(path, text);
        self.generations.insert(claim_id, generation);
        for input in inputs {
            self.inputs
                .insert((input.source.clone(), input.key.clone()), input);
        }
        true
    }
}

impl Database {
    pub fn prepare_publication(
        &mut self,
        repository: &History,
        runtime: &Runtime,
        batch_size: usize,
    ) -> Result<Option<PendingPublication>> {
        self.prepare_for(
            repository,
            runtime,
            batch_size,
            &repository.path().to_string_lossy(),
            None,
        )
    }

    fn prepare_for(
        &mut self,
        repository: &History,
        runtime: &Runtime,
        batch_size: usize,
        destination: &str,
        remote_base: Option<&str>,
    ) -> Result<Option<PendingPublication>> {
        ensure!(
            (1..=999).contains(&batch_size),
            "publication batch size must be between 1 and 999"
        );
        let mut transaction = self.client.transaction()?;
        transaction.query_one("SELECT pg_advisory_xact_lock($1)", &[&LOCK])?;
        if let Some(previous) = transaction.query_opt(
            "SELECT repository FROM corpus.publications ORDER BY id DESC LIMIT 1",
            &[],
        )? {
            ensure!(
                previous.get::<_, String>(0) == destination,
                "this database already has a different canonical publication destination"
            );
        }
        let existing = transaction.query_opt("SELECT id,prepared,generations FROM corpus.publications WHERE repository=$1 AND NOT acknowledged", &[&destination])?;
        if let Some(existing) = existing {
            let generations: Value = existing.get(2);
            let pending = PendingPublication {
                id: existing.get(0),
                prepared: serde_json::from_value(existing.get(1))?,
                remote_base: generations["remote_base"].as_str().map(str::to_owned),
            };
            transaction.commit()?;
            return Ok(Some(pending));
        }
        ensure!(
            transaction
                .query_opt(
                    "SELECT 1 FROM corpus.jobs WHERE scope>='policy:' AND scope<'policy;' LIMIT 1",
                    &[]
                )?
                .is_none(),
            "run corpus sync before publishing changed native resolutions or policy"
        );
        let conflicting: bool = transaction.query_one("SELECT EXISTS(SELECT 1 FROM corpus.footprints GROUP BY origin,target_source HAVING count(DISTINCT target)>1)", &[])?.get(0);
        ensure!(
            !conflicting,
            "accepted claims violate the explicit directional conflict policy; review the conflicting claims"
        );
        let policy = transaction.query_one(
            "SELECT document,generation,published_generation FROM corpus.policy WHERE singleton",
            &[],
        )?;
        let policy_changed = policy.get::<_, i64>(1) != policy.get::<_, i64>(2);
        let rows = transaction.query("WITH pending AS (SELECT p.claim_id,p.generation,d.record,d.action FROM corpus.pending_publication p JOIN corpus.decisions d USING(claim_id) ORDER BY p.claim_id LIMIT $1 FOR UPDATE OF p), bounded AS (SELECT *,sum(octet_length(record)) OVER(ORDER BY claim_id) bytes FROM pending) SELECT claim_id,generation,record,action FROM bounded WHERE bytes<=6291456 ORDER BY claim_id", &[&(batch_size as i64)])?;
        if rows.is_empty() && !policy_changed {
            transaction.commit()?;
            return Ok(None);
        }
        let mut batch = PublicationBatch::default();
        for row in rows {
            let claim_id: String = row.get(0);
            let generation: i64 = row.get(1);
            let mut record = parse_record(&row.get::<_, String>(2))?;
            let action: String = row.get(3);
            let path = claim_path(&record.statement.claim_id());
            let (text, inputs) = if action == "retracted" {
                (None, Vec::new())
            } else {
                let (bindings, inputs) = review::bindings(&mut transaction, &record)?;
                if action == "accepted"
                    && let Err(error) =
                        trakkin_mappings_language::evaluate(&record.statement, &bindings)
                    && error.kind == trakkin_mappings_language::SemanticErrorKind::Invalid
                {
                    anyhow::bail!(
                        "accepted claim {claim_id} is no longer valid: {error}; review it before publication"
                    );
                }
                bindings.write(&mut record)?;
                (
                    Some(
                        Artifact {
                            record,
                            rejected: action == "rejected",
                        }
                        .text(),
                    ),
                    inputs,
                )
            };
            if !batch.push(claim_id, generation, path, text, inputs) {
                break;
            }
        }
        if policy_changed {
            batch.changes.insert(
                POLICY.into(),
                Some(format!(
                    "{}\n",
                    serde_json::to_string_pretty(&policy.get::<_, Value>(0))?
                )),
            );
        }
        let base = runtime.block_on(repository.head())?;
        let digest = sha256(&serde_json::to_vec(&(
            &base,
            &batch.changes,
            &batch.generations,
        ))?);
        let prepared = runtime.block_on(repository.prepare(
            &base,
            &batch.changes,
            &format!("Publish mapping decisions {digest}"),
        ))?;
        let generation_document = json!({"claims":batch.generations,"policy":policy_changed.then(|| policy.get::<_,i64>(1)),"remote_base":remote_base});
        let id: i64 = transaction.query_one("INSERT INTO corpus.publications(repository,prepared,generations) VALUES($1,$2,$3) RETURNING id", &[&destination,&serde_json::to_value(&prepared)?,&generation_document])?.get(0);
        for input in batch.inputs.into_values() {
            transaction.execute("INSERT INTO corpus.publication_inputs(publication_id,source,revision,key) VALUES($1,$2,$3,$4)", &[&id,&input.source,&input.revision,&input.key])?;
        }
        transaction.commit()?;
        Ok(Some(PendingPublication {
            id,
            prepared,
            remote_base: remote_base.map(str::to_owned),
        }))
    }

    pub fn publish(
        &mut self,
        repository: &History,
        runtime: &Runtime,
        batch_size: usize,
    ) -> Result<PublishReport> {
        self.publish_guarded(repository, runtime, batch_size, None)
    }

    pub fn publish_to(
        &mut self,
        repository: &History,
        runtime: &Runtime,
        batch_size: usize,
        remote: &str,
        branch: &str,
    ) -> Result<PublishReport> {
        let remote = crate::git::validate_remote(remote)?;
        self.publish_guarded(repository, runtime, batch_size, Some((&remote, branch)))
    }

    fn publish_guarded(
        &mut self,
        repository: &History,
        runtime: &Runtime,
        batch_size: usize,
        remote: Option<(&str, &str)>,
    ) -> Result<PublishReport> {
        let locked: bool = self
            .client
            .query_one("SELECT pg_try_advisory_lock($1)", &[&(LOCK + 1)])?
            .get(0);
        ensure!(locked, "another publisher is active");
        let result = self.publish_inner(repository, runtime, batch_size, remote);
        self.client
            .query_one("SELECT pg_advisory_unlock($1)", &[&(LOCK + 1)])?;
        result
    }

    fn publish_inner(
        &mut self,
        repository: &History,
        runtime: &Runtime,
        batch_size: usize,
        remote: Option<(&str, &str)>,
    ) -> Result<PublishReport> {
        let mut report = PublishReport::default();
        let destination = remote
            .map(|(url, branch)| format!("{url}#{branch}"))
            .unwrap_or_else(|| repository.path().to_string_lossy().into_owned());
        loop {
            let remote_head = if let Some((url, branch)) = remote {
                let head = runtime.block_on(repository.publication_remote(url, branch))?;
                if let Some(pending)=self.client.query_opt("SELECT prepared,generations FROM corpus.publications WHERE repository=$1 AND NOT acknowledged",&[&destination])? {
                    let prepared:Prepared=serde_json::from_value(pending.get(0))?;
                    let generations:Value=pending.get(1);
                    ensure!(head.as_deref()==Some(&prepared.commit)||head.as_deref()==generations["remote_base"].as_str(),"remote publication branch changed outside the pending publication");
                } else {
                    let previous=self.client.query_opt("SELECT prepared->>'commit' FROM corpus.publications WHERE repository=$1 AND acknowledged ORDER BY id DESC LIMIT 1",&[&destination])?.map(|row|row.get::<_,String>(0));
                    ensure!(head==previous,"remote destination must be empty initially and thereafter match the last acknowledged publication");
                }
                if let Some(commit) = &head {
                    runtime.block_on(repository.follow_publication(commit))?;
                }
                head
            } else {
                None
            };
            let Some(pending) = self.prepare_for(
                repository,
                runtime,
                batch_size,
                &destination,
                remote_head.as_deref(),
            )?
            else {
                break;
            };
            runtime.block_on(repository.apply(&pending.prepared))?;
            if let Some((url, branch)) = remote
                && remote_head.as_deref() != Some(&pending.prepared.commit)
            {
                runtime.block_on(repository.push_publication(
                    url,
                    branch,
                    pending.remote_base.as_deref(),
                    &pending.prepared.commit,
                ))?;
            }
            let mut transaction = self.client.transaction()?;
            transaction.query_one("SELECT pg_advisory_xact_lock($1)", &[&LOCK])?;
            let row = transaction.query_one(
                "SELECT generations FROM corpus.publications WHERE id=$1 FOR UPDATE",
                &[&pending.id],
            )?;
            let generations: Value = row.get(0);
            for (claim_id, generation) in generations["claims"]
                .as_object()
                .context("invalid publication generations")?
            {
                transaction.execute(
                    "DELETE FROM corpus.pending_publication WHERE claim_id=$1 AND generation<=$2",
                    &[
                        claim_id,
                        &generation
                            .as_i64()
                            .context("invalid publication generation")?,
                    ],
                )?;
            }
            if let Some(generation) = generations["policy"].as_i64() {
                transaction.execute("UPDATE corpus.policy SET published_generation=GREATEST(published_generation,$1) WHERE singleton", &[&generation])?;
            }
            transaction.execute(
                "UPDATE corpus.publications SET acknowledged=true WHERE id=$1",
                &[&pending.id],
            )?;
            transaction.commit()?;
            report.batches += 1;
            report.commit = Some(pending.prepared.commit);
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_batch_retains_only_inputs_of_admitted_claims() {
        let mut batch = PublicationBatch::default();
        assert!(batch.push(
            "published".into(),
            1,
            "published.trk".into(),
            Some("ok".into()),
            vec![Input {
                source: "source".into(),
                key: "published".into(),
                revision: 1,
                field: String::new(),
                value: Value::Null,
            }]
        ));
        batch.bytes = 6 * 1024 * 1024;
        assert!(!batch.push(
            "queued".into(),
            2,
            "queued.trk".into(),
            Some("later".into()),
            vec![Input {
                source: "source".into(),
                key: "queued".into(),
                revision: 2,
                field: String::new(),
                value: Value::Null,
            }]
        ));
        assert_eq!(batch.changes.len(), 1);
        assert_eq!(batch.generations, BTreeMap::from([("published".into(), 1)]));
        assert_eq!(batch.inputs.len(), 1);
        assert!(
            batch
                .inputs
                .contains_key(&("source".into(), "published".into()))
        );
    }
}
