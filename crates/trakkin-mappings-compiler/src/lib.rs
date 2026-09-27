use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use trakkin_mappings_language::{
    Expression, ID_VERSION, RELATION_KEY_VERSION, Resolved, ResolvedExpression, Resolver,
    Selection, validate, visit_records,
};

pub const SCHEMA_VERSION: u32 = 1;
pub const COMPILER_VERSION: &str = "trakkin:runtime-compiler:v1\0";
pub const SELECTOR_POLICY_VERSION: &str = "trakkin:selector-policy:none:v1\0";

const SCHEMA: &str = "
PRAGMA foreign_keys = ON;
PRAGMA page_size = 4096;
CREATE TABLE layer (
    position INTEGER PRIMARY KEY,
    source_key TEXT NOT NULL UNIQUE,
    content_hash TEXT NOT NULL
);
CREATE TABLE occurrence (
    id INTEGER PRIMARY KEY,
    layer_position INTEGER NOT NULL REFERENCES layer(position),
    record_ordinal INTEGER NOT NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    start_byte INTEGER NOT NULL,
    end_byte INTEGER NOT NULL,
    statement_id TEXT NOT NULL,
    relation_key TEXT NOT NULL,
    statement TEXT NOT NULL,
    record TEXT NOT NULL,
    operator TEXT NOT NULL,
    left_json TEXT NOT NULL,
    right_json TEXT NOT NULL,
    evidence_fingerprint TEXT NOT NULL,
    UNIQUE(layer_position, record_ordinal),
    UNIQUE(layer_position, relation_key)
);
CREATE TABLE metadata (
    occurrence_id INTEGER NOT NULL REFERENCES occurrence(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL,
    text TEXT NOT NULL,
    annotation_name TEXT,
    annotation_value TEXT,
    PRIMARY KEY(occurrence_id, ordinal)
) WITHOUT ROWID;
CREATE TABLE selection (
    occurrence_id INTEGER NOT NULL REFERENCES occurrence(id) ON DELETE CASCADE,
    side INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    expression_path TEXT NOT NULL,
    reference TEXT NOT NULL,
    source TEXT NOT NULL,
    resolution_key TEXT NOT NULL,
    extent INTEGER NOT NULL,
    PRIMARY KEY(occurrence_id, side, ordinal)
) WITHOUT ROWID;
CREATE TABLE resolved_unit (
    occurrence_id INTEGER NOT NULL REFERENCES occurrence(id) ON DELETE CASCADE,
    side INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    unit TEXT NOT NULL,
    extent INTEGER NOT NULL,
    coordinate TEXT,
    PRIMARY KEY(occurrence_id, side, ordinal),
    UNIQUE(occurrence_id, side, unit)
) WITHOUT ROWID;
CREATE TABLE active_relation (
    relation_key TEXT PRIMARY KEY,
    occurrence_id INTEGER NOT NULL UNIQUE REFERENCES occurrence(id)
) WITHOUT ROWID;
CREATE TABLE build_metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
PRAGMA user_version = 1;
";

const INDEXES: &str = "
CREATE INDEX occurrence_statement ON occurrence(statement_id, relation_key);
CREATE INDEX occurrence_relation ON occurrence(relation_key, layer_position, record_ordinal);
CREATE INDEX occurrence_layer ON occurrence(layer_position, record_ordinal);
CREATE INDEX occurrence_operator ON occurrence(operator, relation_key, statement_id);
CREATE INDEX metadata_annotation ON metadata(annotation_name, annotation_value, occurrence_id);
CREATE INDEX selection_reference ON selection(reference, occurrence_id, side, ordinal);
CREATE INDEX selection_source ON selection(source, occurrence_id, side, ordinal);
CREATE INDEX resolved_unit_lookup ON resolved_unit(unit, occurrence_id, side, ordinal);
";

#[derive(Clone, Debug)]
pub struct MappingLayer {
    pub source_key: String,
    pub content_hash: String,
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug)]
pub struct CompileOptions {
    pub max_record_bytes: usize,
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            max_record_bytes: 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompileSummary {
    pub chain_fingerprint: String,
    pub layer_count: u64,
    pub occurrence_count: u64,
    pub active_relation_count: u64,
    pub shadowed_occurrence_count: u64,
}

pub fn compile(
    layers: &[MappingLayer],
    destination: &Path,
    options: CompileOptions,
) -> Result<CompileSummary> {
    ensure!(
        options.max_record_bytes > 0,
        "record size limit must be positive"
    );
    ensure!(
        !destination.exists(),
        "candidate index already exists: {}",
        destination.display()
    );
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating candidate directory {}", parent.display()))?;
    }

    let result = compile_candidate(layers, destination, options);
    if let Err(error) = result {
        if let Err(cleanup_error) = fs::remove_file(destination)
            && cleanup_error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error.context(format!(
                "failed to remove invalid candidate {}: {cleanup_error}",
                destination.display()
            )));
        }
        return Err(error);
    }
    result
}

pub fn open_read_only(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    ensure!(
        version == SCHEMA_VERSION,
        "unsupported runtime index version; rebuild the derived index"
    );
    Ok(connection)
}

fn compile_candidate(
    layers: &[MappingLayer],
    destination: &Path,
    options: CompileOptions,
) -> Result<CompileSummary> {
    validate_layers(layers)?;
    let chain_fingerprint = chain_fingerprint(layers);
    let mut connection = Connection::open(destination)
        .with_context(|| format!("creating candidate index {}", destination.display()))?;
    connection.execute_batch(
        "PRAGMA journal_mode = OFF;
         PRAGMA synchronous = OFF;
         PRAGMA locking_mode = EXCLUSIVE;
         PRAGMA temp_store = FILE;
         PRAGMA cache_size = -65536;",
    )?;
    connection.execute_batch(SCHEMA)?;

    let mut occurrence_count = 0_u64;
    let mut active_relation_count = 0_u64;
    {
        let transaction = connection.transaction()?;
        for (position, layer) in layers.iter().enumerate() {
            transaction
                .prepare_cached(
                    "INSERT INTO layer(position, source_key, content_hash) VALUES (?1, ?2, ?3)",
                )
                .and_then(|mut statement| {
                    statement.execute(params![
                        position as u64,
                        layer.source_key,
                        layer.content_hash
                    ])
                })
                .with_context(|| format!("registering mapping source {}", layer.source_key))?;
            let counts = compile_layer(&transaction, position as u64, layer, options)?;
            occurrence_count += counts.0;
            active_relation_count += counts.1;
        }
        transaction.execute_batch(INDEXES)?;
        for (key, value) in [
            ("chain_fingerprint", chain_fingerprint.clone()),
            ("layer_count", layers.len().to_string()),
            ("occurrence_count", occurrence_count.to_string()),
            ("active_relation_count", active_relation_count.to_string()),
            (
                "shadowed_occurrence_count",
                (occurrence_count - active_relation_count).to_string(),
            ),
            ("selector_policy", SELECTOR_POLICY_VERSION.to_owned()),
        ] {
            transaction
                .prepare_cached("INSERT INTO build_metadata(key, value) VALUES (?1, ?2)")?
                .execute(params![key, value])?;
        }
        transaction.commit()?;
    }

    connection.execute_batch("ANALYZE;")?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    ensure!(
        integrity == "ok",
        "SQLite integrity check failed: {integrity}"
    );
    drop(connection);
    File::open(destination)?.sync_all()?;

    Ok(CompileSummary {
        chain_fingerprint,
        layer_count: layers.len() as u64,
        occurrence_count,
        active_relation_count,
        shadowed_occurrence_count: occurrence_count - active_relation_count,
    })
}

fn validate_layers(layers: &[MappingLayer]) -> Result<()> {
    let mut source_keys = BTreeSet::new();
    for layer in layers {
        ensure!(!layer.source_key.is_empty(), "mapping source key is empty");
        ensure!(
            source_keys.insert(&layer.source_key),
            "duplicate mapping source key {}",
            layer.source_key
        );
        ensure!(
            layer.content_hash.len() == 64
                && layer
                    .content_hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "mapping source {} has an invalid SHA-256 content hash",
            layer.source_key
        );
    }
    Ok(())
}

fn compile_layer(
    transaction: &Transaction<'_>,
    position: u64,
    layer: &MappingLayer,
    options: CompileOptions,
) -> Result<(u64, u64)> {
    let file = File::open(&layer.path)
        .with_context(|| format!("opening mapping source {}", layer.path.display()))?;
    let hashing_reader = HashingReader::new(file);
    let mut reader = BufReader::with_capacity(64 * 1024, hashing_reader);
    let mut occurrence_count = 0_u64;
    let mut active_relation_count = 0_u64;
    visit_records(&mut reader, options.max_record_bytes, |located| {
        let active = insert_occurrence(transaction, position, layer, located)?;
        occurrence_count += 1;
        active_relation_count += u64::from(active);
        Ok(())
    })
    .with_context(|| format!("compiling mapping source {}", layer.source_key))?;
    let content_hash = reader.into_inner().finish();
    ensure!(
        content_hash == layer.content_hash,
        "mapping source {} content hash changed while compiling",
        layer.source_key
    );
    Ok((occurrence_count, active_relation_count))
}

fn insert_occurrence(
    transaction: &Transaction<'_>,
    position: u64,
    layer: &MappingLayer,
    located: trakkin_mappings_language::LocatedRecord,
) -> Result<bool> {
    let statement = &located.record.statement;
    let resolved = validate(statement, &BareReferenceResolver).with_context(|| {
        format!(
            "validating source {} record {}",
            layer.source_key, located.ordinal
        )
    })?;
    let evidence_fingerprint = evidence_fingerprint(&located.record.canonical(), &resolved);
    let left_json = serde_json::to_string(&statement.left)?;
    let right_json = serde_json::to_string(&statement.right)?;
    transaction
        .prepare_cached(
            "INSERT INTO occurrence(
                layer_position, record_ordinal, start_line, end_line, start_byte, end_byte,
                statement_id, relation_key, statement, record, operator, left_json, right_json,
                evidence_fingerprint
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        )
        .and_then(|mut prepared| {
            prepared.execute(params![
                position,
                located.ordinal,
                located.span.start_line,
                located.span.end_line,
                located.span.start_byte,
                located.span.end_byte,
                statement.id(),
                statement.relation_key(),
                statement.canonical(),
                located.record.canonical(),
                statement.operator.text(),
                left_json,
                right_json,
                evidence_fingerprint,
            ])
        })
        .with_context(|| {
            format!(
                "source {} contains duplicate relation key {} at record {}",
                layer.source_key,
                statement.relation_key(),
                located.ordinal
            )
        })?;
    let occurrence_id = transaction.last_insert_rowid();
    insert_metadata(transaction, occurrence_id, &located.record.metadata)?;
    for (side, expression) in [&statement.left, &statement.right].into_iter().enumerate() {
        let mut ordinal = 0_u64;
        insert_selections(
            transaction,
            occurrence_id,
            side as u64,
            expression,
            "$",
            &mut ordinal,
        )?;
    }
    insert_resolved_units(transaction, occurrence_id, 0, &resolved.0)?;
    insert_resolved_units(transaction, occurrence_id, 1, &resolved.1)?;
    let inserted = transaction
        .prepare_cached(
            "INSERT OR IGNORE INTO active_relation(relation_key, occurrence_id) VALUES (?1, ?2)",
        )?
        .execute(params![statement.relation_key(), occurrence_id])?;
    Ok(inserted == 1)
}

fn insert_metadata(
    transaction: &Transaction<'_>,
    occurrence_id: i64,
    metadata: &[String],
) -> Result<()> {
    for (ordinal, text) in metadata.iter().enumerate() {
        let annotation = text
            .strip_prefix("#@")
            .and_then(|text| text.split_once(' '))
            .map(|(name, value)| (name, value.trim_start_matches(' ')));
        transaction
            .prepare_cached(
                "INSERT INTO metadata(occurrence_id, ordinal, text, annotation_name, annotation_value)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![
                occurrence_id,
                ordinal as u64,
                text,
                annotation.map(|value| value.0),
                annotation.map(|value| value.1),
            ])?;
    }
    Ok(())
}

fn insert_selections(
    transaction: &Transaction<'_>,
    occurrence_id: i64,
    side: u64,
    expression: &Expression,
    path: &str,
    ordinal: &mut u64,
) -> Result<()> {
    match expression {
        Expression::Selection(selection) => {
            transaction
                .prepare_cached(
                    "INSERT INTO selection(
                        occurrence_id, side, ordinal, expression_path, reference, source,
                        resolution_key, extent
                     ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                )?
                .execute(params![
                    occurrence_id,
                    side,
                    *ordinal,
                    path,
                    selection.reference,
                    selection.source(),
                    selection.resolution_key(),
                    selection.extent.get(),
                ])?;
            *ordinal += 1;
        }
        Expression::Composite(expressions) => {
            for (index, expression) in expressions.iter().enumerate() {
                insert_selections(
                    transaction,
                    occurrence_id,
                    side,
                    expression,
                    &format!("{path}/{index}"),
                    ordinal,
                )?;
            }
        }
    }
    Ok(())
}

fn insert_resolved_units(
    transaction: &Transaction<'_>,
    occurrence_id: i64,
    side: u64,
    resolved: &ResolvedExpression,
) -> Result<()> {
    for (ordinal, (unit, extent)) in resolved.items.iter().zip(&resolved.extents).enumerate() {
        let coordinate = resolved
            .coordinates
            .as_ref()
            .and_then(|coordinates| coordinates.get(ordinal));
        transaction
            .prepare_cached(
                "INSERT INTO resolved_unit(occurrence_id, side, ordinal, unit, extent, coordinate)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                occurrence_id,
                side,
                ordinal as u64,
                unit,
                extent,
                coordinate,
            ])?;
    }
    Ok(())
}

fn chain_fingerprint(layers: &[MappingLayer]) -> String {
    let mut hasher = Sha256::new();
    hash_part(&mut hasher, b"trakkin:runtime-chain:v1\0");
    hash_part(&mut hasher, COMPILER_VERSION.as_bytes());
    hash_part(&mut hasher, &SCHEMA_VERSION.to_be_bytes());
    hash_part(&mut hasher, ID_VERSION.as_bytes());
    hash_part(&mut hasher, RELATION_KEY_VERSION.as_bytes());
    hash_part(&mut hasher, SELECTOR_POLICY_VERSION.as_bytes());
    for layer in layers {
        hash_part(&mut hasher, layer.source_key.as_bytes());
        hash_part(&mut hasher, layer.content_hash.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn evidence_fingerprint(
    canonical_record: &str,
    resolved: &(ResolvedExpression, ResolvedExpression),
) -> String {
    let mut hasher = Sha256::new();
    hash_part(&mut hasher, b"trakkin:occurrence-evidence:v1\0");
    hash_part(&mut hasher, canonical_record.as_bytes());
    for (side, expression) in [&resolved.0, &resolved.1].into_iter().enumerate() {
        hash_part(&mut hasher, &(side as u64).to_be_bytes());
        for (ordinal, (unit, extent)) in
            expression.items.iter().zip(&expression.extents).enumerate()
        {
            hash_part(&mut hasher, &(ordinal as u64).to_be_bytes());
            hash_part(&mut hasher, unit.as_bytes());
            hash_part(&mut hasher, &extent.to_be_bytes());
            if let Some(coordinate) = expression
                .coordinates
                .as_ref()
                .and_then(|coordinates| coordinates.get(ordinal))
            {
                hash_part(&mut hasher, coordinate.as_bytes());
            } else {
                hash_part(&mut hasher, &[]);
            }
        }
    }
    format!("{:x}", hasher.finalize())
}

fn hash_part(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

struct BareReferenceResolver;

impl Resolver for BareReferenceResolver {
    fn resolve(&self, selection: &Selection) -> Result<Resolved> {
        ensure!(
            selection.selector.is_none(),
            "selectors are unsupported until versioned adapter evidence is configured"
        );
        Ok(Resolved {
            items: vec![selection.reference.clone()],
            ordered: true,
            coordinates: None,
        })
    }
}

struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R> HashingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    fn finish(self) -> String {
        format!("{:x}", self.hasher.finalize())
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let length = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..length]);
        Ok(length)
    }
}
