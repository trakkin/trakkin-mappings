use anyhow::{Result, ensure};
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};
use trakkin_mappings_language::{Record, Resolved};

pub const SCHEMA_VERSION: u32 = 1;

const SCHEMA: &str = "
PRAGMA foreign_keys = ON;
PRAGMA page_size = 4096;
CREATE TABLE IF NOT EXISTS shard (path TEXT PRIMARY KEY, content_hash TEXT NOT NULL, adapter_hash TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS mapping (
    id TEXT PRIMARY KEY, relation_key TEXT NOT NULL, shard TEXT NOT NULL REFERENCES shard(path) ON DELETE CASCADE,
    statement TEXT NOT NULL, record TEXT NOT NULL, operator TEXT NOT NULL, left_json TEXT NOT NULL, right_json TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS mapping_relation ON mapping(relation_key);
CREATE INDEX IF NOT EXISTS mapping_shard ON mapping(shard, id);
CREATE TABLE IF NOT EXISTS annotation (
    mapping_id TEXT NOT NULL REFERENCES mapping(id) ON DELETE CASCADE, ordinal INTEGER NOT NULL, name TEXT NOT NULL, value TEXT NOT NULL,
    PRIMARY KEY(mapping_id, ordinal)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS endpoint (
    mapping_id TEXT NOT NULL REFERENCES mapping(id) ON DELETE CASCADE, side INTEGER NOT NULL,
    reference TEXT NOT NULL, source TEXT NOT NULL, PRIMARY KEY(mapping_id, side, reference)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS endpoint_reference ON endpoint(reference, mapping_id);
CREATE INDEX IF NOT EXISTS endpoint_source ON endpoint(source, mapping_id);
CREATE TABLE IF NOT EXISTS claim (
    mapping_id TEXT NOT NULL REFERENCES mapping(id) ON DELETE CASCADE, origin TEXT NOT NULL,
    target_source TEXT NOT NULL, target_unit TEXT NOT NULL, exclusive INTEGER NOT NULL,
    PRIMARY KEY(mapping_id, origin, target_source, target_unit)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS claim_origin ON claim(origin, target_source, target_unit);
CREATE TABLE IF NOT EXISTS build_metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
PRAGMA user_version = 1;
";

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let connection = Connection::open(path)?;
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    ensure!(
        version == 0 || version == SCHEMA_VERSION,
        "unsupported SQLite index version; rebuild the derived index"
    );
    connection.execute_batch(SCHEMA)?;
    Ok(connection)
}

pub fn shard_keys(connection: &Connection) -> Result<BTreeMap<String, (String, String)>> {
    Ok(connection
        .prepare("SELECT path, content_hash, adapter_hash FROM shard ORDER BY path")?
        .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn insert_record(
    connection: &Connection,
    path: &str,
    record: &Record,
    resolved: &(Resolved, Resolved),
    exclusive: (bool, bool),
) -> Result<()> {
    let statement = &record.statement;
    let id = statement.id();
    connection.execute(
        "INSERT INTO mapping VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            id,
            statement.relation_key(),
            path,
            statement.canonical(),
            record.canonical(),
            statement.operator.text(),
            serde_json::to_string(&statement.left)?,
            serde_json::to_string(&statement.right)?
        ],
    )?;
    for (ordinal, annotation) in record.metadata.iter().enumerate() {
        let (name, value) = annotation
            .strip_prefix("#@")
            .unwrap()
            .split_once(' ')
            .unwrap();
        connection.execute(
            "INSERT INTO annotation VALUES (?1,?2,?3,?4)",
            params![id, ordinal as u64, name, value],
        )?;
    }
    for (side, expression) in [&statement.left, &statement.right].iter().enumerate() {
        for selection in expression.selections() {
            connection.execute(
                "INSERT OR IGNORE INTO endpoint VALUES (?1,?2,?3,?4)",
                params![id, side as u64, selection.reference, selection.source()],
            )?;
        }
    }
    if statement.operator == trakkin_mappings_language::Operator::Exact {
        let left_source = statement.left.selections()[0].source();
        let right_source = statement.right.selections()[0].source();
        for (left, right) in resolved.0.items.iter().zip(&resolved.1.items) {
            for (origin, target_source, target, exclusive) in [
                (left, right_source, right, exclusive.0),
                (right, left_source, left, exclusive.1),
            ] {
                connection.execute(
                    "INSERT INTO claim VALUES (?1,?2,?3,?4,?5) ON CONFLICT(mapping_id, origin, target_source, target_unit) DO NOTHING",
                    params![id, origin, target_source, target, exclusive],
                )?;
            }
        }
    }
    Ok(())
}

pub fn check_conflicts(connection: &Connection) -> Result<()> {
    let duplicate: Option<String> = connection.prepare("SELECT group_concat(id) FROM mapping GROUP BY relation_key HAVING count(*) > 1 LIMIT 1")?.query_map([], |row| row.get(0))?.next().transpose()?;
    ensure!(
        duplicate.is_none(),
        "duplicate symmetric relationship: {}",
        duplicate.unwrap_or_default()
    );
    let conflict: Option<String> = connection.prepare("SELECT origin || ' -> ' || target_source || ': ' || group_concat(DISTINCT mapping_id) FROM claim GROUP BY origin, target_source HAVING count(DISTINCT target_unit) > 1 AND max(exclusive) = 1 LIMIT 1")?.query_map([], |row| row.get(0))?.next().transpose()?;
    ensure!(
        conflict.is_none(),
        "adapter-declared exclusive mapping conflict: {}",
        conflict.unwrap_or_default()
    );
    Ok(())
}

pub fn overlap_warnings(connection: &Connection) -> Result<Vec<String>> {
    Ok(connection.prepare("SELECT reference || ': ' || count(DISTINCT mapping_id) || ' mappings share a reference; review selector/coverage overlap' FROM endpoint GROUP BY reference HAVING count(DISTINCT mapping_id) > 1 ORDER BY reference LIMIT 100")?.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?)
}

pub fn logical_hash(connection: &Connection) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"trakkin:index:v1\0");
    for query in [
        "SELECT id, relation_key, shard, statement, record, operator, left_json, right_json FROM mapping ORDER BY id",
        "SELECT mapping_id, cast(ordinal AS TEXT), name, value FROM annotation ORDER BY mapping_id, ordinal",
        "SELECT mapping_id, cast(side AS TEXT), reference, source FROM endpoint ORDER BY mapping_id, side, reference",
        "SELECT mapping_id, origin, target_source, target_unit, cast(exclusive AS TEXT) FROM claim ORDER BY mapping_id, origin, target_source, target_unit",
        "SELECT path, content_hash, adapter_hash FROM shard ORDER BY path",
    ] {
        hasher.update(query.as_bytes());
        let mut statement = connection.prepare(query)?;
        let columns = statement.column_count();
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            for index in 0..columns {
                let value: String = row.get(index)?;
                hasher.update((value.len() as u64).to_be_bytes());
                hasher.update(value.as_bytes());
            }
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn snapshot(
    source: &Path,
    destination: &Path,
    commit: &str,
    dirty: bool,
    adapters: &str,
) -> Result<()> {
    let connection = open(destination)?;
    connection.execute("ATTACH DATABASE ?1 AS cached", [source.to_str().unwrap()])?;
    connection.execute_batch("BEGIN;
        INSERT INTO shard SELECT * FROM cached.shard ORDER BY path;
        INSERT INTO mapping SELECT * FROM cached.mapping ORDER BY id;
        INSERT INTO annotation SELECT * FROM cached.annotation ORDER BY mapping_id, ordinal;
        INSERT INTO endpoint SELECT * FROM cached.endpoint ORDER BY mapping_id, side, reference;
        INSERT INTO claim SELECT * FROM cached.claim ORDER BY mapping_id, origin, target_source, target_unit;
        COMMIT;")?;
    for (key, value) in [
        ("source_commit", commit.to_owned()),
        ("dirty", dirty.to_string()),
        ("canonicalization", "v1".into()),
        ("adapters_sha256", adapters.to_owned()),
        ("logical_sha256", logical_hash(&connection)?),
    ] {
        connection.execute("INSERT INTO build_metadata VALUES (?1, ?2)", [key, &value])?;
    }
    connection.execute_batch("DETACH DATABASE cached; VACUUM;")?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    ensure!(
        integrity == "ok",
        "SQLite integrity check failed: {integrity}"
    );
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub id: String,
    pub statement: String,
    pub record: String,
}

pub fn query(path: &Path, reference: &str, limit: usize) -> Result<Vec<SearchResult>> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    ensure!(
        (1..=1000).contains(&limit),
        "query limit must be between 1 and 1000"
    );
    Ok(connection.prepare("SELECT id, statement, record FROM mapping WHERE id IN (SELECT mapping_id FROM endpoint WHERE reference = ?1) ORDER BY id LIMIT ?2")?.query_map(params![reference, limit as u64], |row| Ok(SearchResult { id: row.get(0)?, statement: row.get(1)?, record: row.get(2)? }))?.collect::<rusqlite::Result<_>>()?)
}
