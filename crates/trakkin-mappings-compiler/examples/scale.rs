use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use trakkin_mappings_compiler::{
    CompileOptions, MappingLayer, MappingQuery, compile, list_mappings, open_read_only,
};

const DEFAULT_OCCURRENCES: u64 = 5_000_000;
const LATENCY_SAMPLES: usize = 20;
const MAX_QUERY_P95: Duration = Duration::from_millis(250);
const MAX_PEAK_RSS_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Serialize)]
struct ScaleReport {
    mapping_occurrences: u64,
    active_relations: u64,
    shadowed_occurrences: u64,
    resolved_units: u64,
    active_exact_claims: u64,
    active_search_documents: u64,
    source_bytes: u64,
    index_bytes: u64,
    build_milliseconds: u64,
    cold_page_microseconds: u64,
    warm_page_p95_microseconds: u64,
    search_p95_microseconds: u64,
    shadowed_detail_microseconds: u64,
    peak_rss_bytes: Option<u64>,
    chain_fingerprint: String,
}

fn main() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let mapping_occurrences = arguments
        .next()
        .map(|value| value.parse::<u64>().context("invalid occurrence count"))
        .transpose()?
        .unwrap_or(DEFAULT_OCCURRENCES);
    ensure!(arguments.next().is_none(), "expected at most one argument");
    ensure!(
        mapping_occurrences >= 1_000 && mapping_occurrences.is_multiple_of(2),
        "occurrence count must be an even number of at least 1,000"
    );
    let unique_relations = mapping_occurrences / 2;
    let temporary = benchmark_tempdir()?;
    let high_path = temporary.path().join("high.trakkin");
    let low_path = temporary.path().join("low.trakkin");
    let (high_hash, high_bytes) = write_layer(&high_path, unique_relations, false)?;
    let (low_hash, low_bytes) = write_layer(&low_path, unique_relations, true)?;
    let index_path = temporary.path().join("runtime.sqlite");

    let started = Instant::now();
    let summary = compile(
        &[
            MappingLayer {
                source_key: "high".into(),
                content_hash: high_hash,
                path: high_path,
            },
            MappingLayer {
                source_key: "low".into(),
                content_hash: low_hash,
                path: low_path,
            },
        ],
        &index_path,
        CompileOptions::default(),
    )?;
    let build_elapsed = started.elapsed();
    ensure!(
        summary.occurrence_count == mapping_occurrences,
        "unexpected occurrence count"
    );
    ensure!(
        summary.active_relation_count == unique_relations,
        "unexpected active relation count"
    );
    ensure!(
        summary.shadowed_occurrence_count == unique_relations,
        "unexpected shadowed occurrence count"
    );
    ensure!(
        summary.active_exact_claim_count == mapping_occurrences,
        "unexpected active exact claim count"
    );

    let connection = open_read_only(&index_path)?;
    validate_query_plans(&connection)?;
    let resolved_units: u64 =
        connection.query_row("SELECT count(*) FROM resolved_unit", [], |row| row.get(0))?;
    let active_search_documents: u64 =
        connection.query_row("SELECT count(*) FROM active_search", [], |row| row.get(0))?;
    ensure!(
        resolved_units == mapping_occurrences * 2,
        "unexpected resolved unit count"
    );
    ensure!(
        active_search_documents == unique_relations,
        "unexpected active search document count"
    );

    let page_query = MappingQuery {
        limit: 100,
        ..MappingQuery::default()
    };
    let started = Instant::now();
    let cold_page = list_mappings(&connection, &page_query, None)?;
    let cold_page_elapsed = started.elapsed();
    ensure!(cold_page.items.len() == 100, "unexpected cold page size");
    let warm_page_p95 = measure_p95(|| {
        let warm_page = list_mappings(&connection, &page_query, None)?;
        ensure!(
            warm_page.items == cold_page.items,
            "warm page changed results"
        );
        Ok(())
    })?;
    validate_pagination(&connection, &page_query)?;

    let search_query = MappingQuery {
        search: Some("item".into()),
        limit: 100,
        ..MappingQuery::default()
    };
    let search_p95 = measure_p95(|| {
        let search_page = list_mappings(&connection, &search_query, None)?;
        ensure!(
            search_page.items.len() == 100,
            "unexpected search result count"
        );
        Ok(())
    })?;
    validate_pagination(&connection, &search_query)?;

    let started = Instant::now();
    let shadowed_page = list_mappings(
        &connection,
        &MappingQuery {
            relation_key: Some(cold_page.items[0].relation_key.clone()),
            include_shadowed: true,
            limit: 10,
            ..MappingQuery::default()
        },
        None,
    )?;
    let shadowed_elapsed = started.elapsed();
    ensure!(
        shadowed_page.items.len() == 2,
        "unexpected provenance occurrence count"
    );

    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    ensure!(
        integrity == "ok",
        "SQLite integrity check failed: {integrity}"
    );
    ensure!(
        warm_page_p95 <= MAX_QUERY_P95,
        "warm page p95 exceeded {} ms: {} ms",
        MAX_QUERY_P95.as_millis(),
        warm_page_p95.as_millis()
    );
    ensure!(
        search_p95 <= MAX_QUERY_P95,
        "search p95 exceeded {} ms: {} ms",
        MAX_QUERY_P95.as_millis(),
        search_p95.as_millis()
    );
    let peak_rss_bytes = peak_rss_bytes();
    if let Some(peak_rss_bytes) = peak_rss_bytes {
        ensure!(
            peak_rss_bytes <= MAX_PEAK_RSS_BYTES,
            "peak RSS exceeded {} bytes: {} bytes",
            MAX_PEAK_RSS_BYTES,
            peak_rss_bytes
        );
    }
    let report = ScaleReport {
        mapping_occurrences,
        active_relations: summary.active_relation_count,
        shadowed_occurrences: summary.shadowed_occurrence_count,
        resolved_units,
        active_exact_claims: summary.active_exact_claim_count,
        active_search_documents,
        source_bytes: high_bytes + low_bytes,
        index_bytes: fs::metadata(&index_path)?.len(),
        build_milliseconds: milliseconds(build_elapsed),
        cold_page_microseconds: microseconds(cold_page_elapsed),
        warm_page_p95_microseconds: microseconds(warm_page_p95),
        search_p95_microseconds: microseconds(search_p95),
        shadowed_detail_microseconds: microseconds(shadowed_elapsed),
        peak_rss_bytes,
        chain_fingerprint: summary.chain_fingerprint,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn write_layer(path: &Path, count: u64, reversed: bool) -> Result<(String, u64)> {
    let file = File::create(path)?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, file);
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    for index in 0..count {
        let line = if reversed {
            format!("b.scale://item/{index} <=> a.scale://item/{index}\n")
        } else {
            format!("a.scale://item/{index} <=> b.scale://item/{index}\n")
        };
        writer.write_all(line.as_bytes())?;
        hasher.update(line.as_bytes());
        bytes += line.len() as u64;
        if index > 0 && index % 100_000 == 0 {
            eprintln!("wrote {} records to {}", index, path.display());
        }
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok((format!("{:x}", hasher.finalize()), bytes))
}

fn benchmark_tempdir() -> Result<TempDir> {
    let root = std::env::var_os("TRAKKIN_SCALE_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("target/scale")
        });
    fs::create_dir_all(&root)
        .with_context(|| format!("creating benchmark directory {}", root.display()))?;
    TempDir::new_in(&root)
        .with_context(|| format!("creating benchmark workspace in {}", root.display()))
}

fn peak_rss_bytes() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let kilobytes = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    kilobytes.checked_mul(1024)
}

fn validate_query_plans(connection: &Connection) -> Result<()> {
    let page_plan = query_plan(
        connection,
        "SELECT occurrence.relation_key
         FROM active_relation
         JOIN occurrence ON occurrence.id = active_relation.occurrence_id
         ORDER BY active_relation.relation_key
         LIMIT 101",
    )?;
    ensure!(
        !page_plan.contains("TEMP B-TREE"),
        "active page query requires a temporary sort:\n{page_plan}"
    );

    let search_plan = query_plan(
        connection,
        "SELECT occurrence.id
         FROM active_search
         JOIN occurrence ON occurrence.id = active_search.rowid
            JOIN active_relation ON active_relation.occurrence_id = occurrence.id
            JOIN layer ON layer.position = occurrence.layer_position
            WHERE active_search MATCH '\"item\"*'
            ORDER BY active_search.rowid
            LIMIT 101",
    )?;
    ensure!(
        search_plan.contains("VIRTUAL TABLE INDEX"),
        "search query does not use the FTS index:\n{search_plan}"
    );
    ensure!(
        !search_plan.contains("TEMP B-TREE"),
        "search query requires a temporary sort:\n{search_plan}"
    );
    Ok(())
}

fn query_plan(connection: &Connection, sql: &str) -> Result<String> {
    let mut statement = connection.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
    let plan = statement
        .query_map([], |row| row.get::<_, String>(3))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .join("\n");
    Ok(plan)
}

fn validate_pagination(connection: &Connection, query: &MappingQuery) -> Result<()> {
    let mut cursor = None;
    let mut relation_keys = BTreeSet::new();
    for _ in 0..3 {
        let page = list_mappings(connection, query, cursor.as_ref())?;
        ensure!(
            page.items.len() == query.limit as usize,
            "unexpected keyset page size"
        );
        for item in page.items {
            ensure!(
                relation_keys.insert(item.relation_key),
                "keyset pagination returned a duplicate relation"
            );
        }
        cursor = page.next_cursor;
        ensure!(cursor.is_some(), "keyset pagination ended early");
    }
    Ok(())
}

fn measure_p95(mut operation: impl FnMut() -> Result<()>) -> Result<Duration> {
    let mut samples = Vec::with_capacity(LATENCY_SAMPLES);
    for _ in 0..LATENCY_SAMPLES {
        let started = Instant::now();
        operation()?;
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    Ok(samples[(LATENCY_SAMPLES * 95).div_ceil(100) - 1])
}

fn milliseconds(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn microseconds(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}
