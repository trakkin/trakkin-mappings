use super::{Input, add_support, review, support_id};
use crate::adapters::{EpisodeOrder, Tmdb, Tvdb};
use anyhow::{Context, Result};
use postgres::Transaction;
use std::collections::BTreeMap;
use trakkin_mappings_language::{Operator, parse_record};

pub(super) fn invalidate(transaction: &mut Transaction<'_>, endpoints: &[String]) -> Result<()> {
    let decisions = transaction.query("SELECT DISTINCT d.claim_id,d.version FROM corpus.decisions d JOIN corpus.decision_endpoints e USING(claim_id) WHERE d.action='accepted' AND e.uri=ANY($1)", &[&endpoints])?;
    for row in decisions {
        let id: String = row.get(0);
        let version: i64 = row.get(1);
        let withdrawn = transaction.query("DELETE FROM corpus.supports s USING corpus.support_decisions dependency WHERE dependency.support_id=s.id AND dependency.version=$1 RETURNING s.claim_id", &[&version])?;
        for row in withdrawn {
            transaction.execute(
                "UPDATE corpus.candidates SET revision=revision+1 WHERE claim_id=$1",
                &[&row.get::<_, String>(0)],
            )?;
        }
        transaction.execute("INSERT INTO corpus.jobs(scope) VALUES($1) ON CONFLICT(scope) DO UPDATE SET after_left='',after_right='',diagnostic=NULL", &[&format!("parent:{id}")])?;
    }
    Ok(())
}

pub(super) fn page(
    transaction: &mut Transaction<'_>,
    claim_id: &str,
    after: &str,
    limit: usize,
) -> Result<(Option<String>, u64)> {
    let Some(decision) = transaction.query_opt(
        "SELECT record,version FROM corpus.decisions WHERE claim_id=$1 AND action='accepted'",
        &[&claim_id],
    )?
    else {
        return Ok((None, 0));
    };
    let record = parse_record(&decision.get::<_, String>(0))?;
    if record.statement.operator != Operator::Exact {
        return Ok((None, 0));
    }
    let left = record.statement.left.selections();
    let right = record.statement.right.selections();
    if left.len() != 1
        || right.len() != 1
        || left[0].selector.is_some()
        || right[0].selector.is_some()
    {
        return Ok((None, 0));
    }
    let (tmdb, tvdb) = if left[0].reference.starts_with("org.themoviedb://tv/")
        && right[0].reference.starts_with("com.thetvdb://series/")
    {
        (&left[0].reference, &right[0].reference)
    } else if right[0].reference.starts_with("org.themoviedb://tv/")
        && left[0].reference.starts_with("com.thetvdb://series/")
    {
        (&right[0].reference, &left[0].reference)
    } else {
        return Ok((None, 0));
    };
    let pending: bool = transaction.query_one("SELECT EXISTS(SELECT 1 FROM corpus.consumers c JOIN ingestion.sources s USING(source) WHERE c.source IN ('org.themoviedb','com.thetvdb') AND (c.bootstrap OR c.after_key IS NOT NULL OR c.revision<s.revision))", &[])?.get(0);
    if pending {
        return Ok((Some(after.into()), 0));
    }
    let (_, left_records, left_inputs) = review::scope_records(transaction, tmdb)?;
    let (_, right_records, right_inputs) = review::scope_records(transaction, tvdb)?;
    let mut left = Tmdb::default_order(tmdb, &left_records)?;
    let mut right = Tvdb::default_order(tvdb, &right_records)?;
    let left_native = left_inputs
        .iter()
        .map(|input| (input.key.as_str(), input))
        .collect();
    let right_native = right_inputs
        .iter()
        .map(|input| (input.key.as_str(), input))
        .collect();
    let mut alignments = super::alignment::align(&mut left, &mut right)?;
    alignments.sort_by_key(|alignment| alignment.record.statement.claim_id());
    let selected = alignments
        .into_iter()
        .filter(|alignment| alignment.record.statement.claim_id().to_string().as_str() > after)
        .take(limit + 1)
        .collect::<Vec<_>>();
    let more = selected.len() > limit;
    let version: i64 = decision.get(1);
    let method = format!("default-order-structure:{version}");
    let mut supports = 0;
    let mut last = None;
    for alignment in selected.into_iter().take(limit) {
        let record = alignment.record;
        last = Some(record.statement.claim_id().to_string());
        let mut inputs = evidence(
            &left,
            alignment.matches.iter().map(|matched| matched.left_index),
            &left_native,
        )?;
        inputs.extend(evidence(
            &right,
            alignment.matches.iter().map(|matched| matched.right_index),
            &right_native,
        )?);
        inputs.sort_by(|left, right| {
            (&left.source, &left.key, &left.field).cmp(&(&right.source, &right.key, &right.field))
        });
        let id = support_id(&record, &method, &inputs)?;
        if add_support(transaction, &record, &method, &inputs)? {
            supports += 1;
        }
        transaction.execute("INSERT INTO corpus.support_decisions(support_id,version) VALUES($1,$2) ON CONFLICT DO NOTHING", &[&id,&version])?;
    }
    Ok((if more { last } else { None }, supports))
}

fn evidence(
    order: &EpisodeOrder,
    indices: impl Iterator<Item = usize>,
    records: &BTreeMap<&str, &Input>,
) -> Result<Vec<Input>> {
    let observations = order
        .observations
        .iter()
        .chain(indices.flat_map(|index| &order.episodes[index].observations));
    let mut inputs = BTreeMap::new();
    for (key, observation) in observations {
        let native = records
            .get(key.as_str())
            .context("native episode evidence record is absent")?;
        inputs.insert(
            (key.clone(), observation.field.clone()),
            Input {
                source: native.source.clone(),
                key: key.clone(),
                revision: native.revision,
                field: observation.field.clone(),
                value: observation.value.clone(),
            },
        );
    }
    Ok(inputs.into_values().collect())
}
