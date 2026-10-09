use super::*;

#[derive(Clone, Debug, Default, Serialize)]
pub struct RetentionReport {
    pub versions: u64,
    pub publications: u64,
}

impl Database {
    pub fn retain(
        &mut self,
        keep_days: i32,
        keep_publications: i64,
        batch_size: usize,
    ) -> Result<RetentionReport> {
        ensure!(
            keep_days >= 1 && keep_publications >= 1 && (1..=1000).contains(&batch_size),
            "retain at least one day and publication; batch size must be between 1 and 1000"
        );
        let mut transaction = self.client.transaction()?;
        transaction.query_one("SELECT pg_advisory_xact_lock($1)", &[&LOCK])?;
        transaction.execute("INSERT INTO corpus.consumers(source,revision) SELECT source,revision FROM ingestion.sources ON CONFLICT DO NOTHING", &[])?;
        let publications = transaction.execute("DELETE FROM corpus.publications p WHERE p.id IN (SELECT id FROM (SELECT id,acknowledged,row_number() OVER(PARTITION BY repository ORDER BY id DESC) position FROM corpus.publications) ranked WHERE acknowledged AND position>$1 ORDER BY id LIMIT $2)", &[&keep_publications,&(batch_size as i64)])?;
        let deleted = transaction.query("WITH victims AS (SELECT v.source,v.revision,v.key FROM ingestion.record_versions v JOIN corpus.consumers c USING(source) WHERE v.revision<c.revision AND v.recorded_at<clock_timestamp()-make_interval(days=>$1) AND NOT EXISTS(SELECT 1 FROM ingestion.records r WHERE (r.source,r.revision,r.key)=(v.source,v.revision,v.key)) AND NOT EXISTS(SELECT 1 FROM corpus.records r WHERE (r.source,r.revision,r.key)=(v.source,v.revision,v.key)) AND NOT EXISTS(SELECT 1 FROM corpus.support_inputs i WHERE (i.source,i.revision,i.key)=(v.source,v.revision,v.key)) AND NOT EXISTS(SELECT 1 FROM corpus.decision_inputs i WHERE (i.source,i.revision,i.key)=(v.source,v.revision,v.key)) AND NOT EXISTS(SELECT 1 FROM corpus.publication_inputs i WHERE (i.source,i.revision,i.key)=(v.source,v.revision,v.key)) ORDER BY v.source,v.revision,v.key LIMIT $2 FOR UPDATE OF v) DELETE FROM ingestion.record_versions v USING victims WHERE (v.source,v.revision,v.key)=(victims.source,victims.revision,victims.key) RETURNING v.source,v.revision", &[&keep_days,&(batch_size as i64)])?;
        let mut floors = BTreeMap::<String, i64>::new();
        for row in &deleted {
            let source = row.get(0);
            let revision = row.get(1);
            floors
                .entry(source)
                .and_modify(|floor| *floor = (*floor).max(revision))
                .or_insert(revision);
        }
        for (source, floor) in floors {
            transaction.execute("UPDATE ingestion.sources SET history_floor=GREATEST(history_floor,$2) WHERE source=$1", &[&source,&floor])?;
        }
        transaction.commit()?;
        Ok(RetentionReport {
            versions: deleted.len() as u64,
            publications,
        })
    }
}
