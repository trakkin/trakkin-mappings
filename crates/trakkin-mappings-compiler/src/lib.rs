use anyhow::{Context, Result, ensure};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, params, params_from_iter,
    types::Value as SqlValue,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use trakkin_mappings_language::{
    Expression, ID_VERSION, Operator, RELATION_KEY_VERSION, Resolved, ResolvedExpression, Resolver,
    Selection, parse, validate, visit_records,
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
    source TEXT NOT NULL,
    extent INTEGER NOT NULL,
    coordinate TEXT,
    PRIMARY KEY(occurrence_id, side, ordinal),
    UNIQUE(occurrence_id, side, unit)
) WITHOUT ROWID;
CREATE TABLE active_relation (
    relation_key TEXT PRIMARY KEY,
    occurrence_id INTEGER NOT NULL UNIQUE REFERENCES occurrence(id)
) WITHOUT ROWID;
CREATE TABLE exclusive_source_pair (
    origin_source TEXT NOT NULL,
    target_source TEXT NOT NULL,
    PRIMARY KEY(origin_source, target_source)
) WITHOUT ROWID;
CREATE TABLE active_exact_claim (
    origin_source TEXT NOT NULL,
    origin_unit TEXT NOT NULL,
    target_source TEXT NOT NULL,
    target_unit TEXT NOT NULL,
    occurrence_id INTEGER NOT NULL REFERENCES occurrence(id),
    PRIMARY KEY(origin_source, origin_unit, target_source, target_unit, occurrence_id)
) WITHOUT ROWID;
CREATE VIRTUAL TABLE active_search USING fts5(
    statement,
    endpoints,
    annotations,
    content=''
);
CREATE TABLE build_metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
PRAGMA user_version = 1;
";

const INDEXES: &str = "
CREATE INDEX occurrence_statement ON occurrence(statement_id, relation_key);
CREATE INDEX occurrence_relation ON occurrence(
    relation_key, layer_position, statement_id, record_ordinal
);
CREATE INDEX occurrence_layer ON occurrence(layer_position, record_ordinal);
CREATE INDEX occurrence_operator ON occurrence(operator, relation_key, statement_id);
CREATE INDEX metadata_annotation ON metadata(annotation_name, annotation_value, occurrence_id);
CREATE INDEX selection_reference ON selection(reference, occurrence_id, side, ordinal);
CREATE INDEX selection_source ON selection(source, occurrence_id, side, ordinal);
CREATE INDEX resolved_unit_lookup ON resolved_unit(unit, occurrence_id, side, ordinal);
CREATE INDEX active_exact_claim_policy ON active_exact_claim(
    origin_source, target_source, origin_unit, target_unit, occurrence_id
);
";

#[derive(Clone, Debug)]
pub struct MappingLayer {
    pub source_key: String,
    pub content_hash: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExclusiveSourcePair {
    pub origin_source: String,
    pub target_source: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictPolicy {
    pub exclusive_source_pairs: BTreeSet<ExclusiveSourcePair>,
}

impl ConflictPolicy {
    pub fn new<I, O, T>(pairs: I) -> Result<Self>
    where
        I: IntoIterator<Item = (O, T)>,
        O: Into<String>,
        T: Into<String>,
    {
        let exclusive_source_pairs = pairs
            .into_iter()
            .map(|(origin_source, target_source)| ExclusiveSourcePair {
                origin_source: origin_source.into(),
                target_source: target_source.into(),
            })
            .collect();
        let policy = Self {
            exclusive_source_pairs,
        };
        policy.validate()?;
        Ok(policy)
    }

    pub fn fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        hash_part(&mut hasher, b"trakkin:conflict-policy:v1\0");
        for pair in &self.exclusive_source_pairs {
            hash_part(&mut hasher, pair.origin_source.as_bytes());
            hash_part(&mut hasher, pair.target_source.as_bytes());
        }
        format!("{:x}", hasher.finalize())
    }

    fn validate(&self) -> Result<()> {
        for pair in &self.exclusive_source_pairs {
            ensure!(
                source_namespace(&pair.origin_source),
                "invalid exclusive origin source {}",
                pair.origin_source
            );
            ensure!(
                source_namespace(&pair.target_source),
                "invalid exclusive target source {}",
                pair.target_source
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct CompileOptions {
    pub max_record_bytes: usize,
    pub conflict_policy: ConflictPolicy,
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            max_record_bytes: 1024 * 1024,
            conflict_policy: ConflictPolicy::default(),
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
    pub active_exact_claim_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingQuery {
    pub search: Option<String>,
    pub canonical_statement: Option<String>,
    pub statement_id: Option<String>,
    pub relation_key: Option<String>,
    pub operator: Option<Operator>,
    pub source_key: Option<String>,
    pub endpoint: Option<String>,
    pub annotation_name: Option<String>,
    pub annotation_value: Option<String>,
    pub include_shadowed: bool,
    pub limit: u32,
}

impl Default for MappingQuery {
    fn default() -> Self {
        Self {
            search: None,
            canonical_statement: None,
            statement_id: None,
            relation_key: None,
            operator: None,
            source_key: None,
            endpoint: None,
            annotation_name: None,
            annotation_value: None,
            include_shadowed: false,
            limit: 50,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingCursor {
    pub chain_fingerprint: String,
    pub query_fingerprint: String,
    pub search_rowid: Option<u64>,
    pub relation_key: String,
    pub layer_position: u64,
    pub statement_id: String,
    pub record_ordinal: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingRow {
    pub relation_key: String,
    pub statement_id: String,
    pub canonical_statement: String,
    pub canonical_record: String,
    pub operator: Operator,
    pub source_key: String,
    pub source_content_hash: String,
    pub layer_position: u64,
    pub record_ordinal: u64,
    pub active: bool,
    pub occurrence_count: u64,
    pub evidence_fingerprint: String,
    pub metadata: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingPage {
    pub chain_fingerprint: String,
    pub items: Vec<MappingRow>,
    pub next_cursor: Option<MappingCursor>,
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

    let result = compile_candidate(layers, destination, &options);
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

pub fn list_mappings(
    connection: &Connection,
    query: &MappingQuery,
    cursor: Option<&MappingCursor>,
) -> Result<MappingPage> {
    ensure!(
        (1..=250).contains(&query.limit),
        "mapping query limit must be between 1 and 250"
    );
    let query = NormalizedQuery::new(query)?;
    let chain_fingerprint: String = connection
        .query_row(
            "SELECT value FROM build_metadata WHERE key = 'chain_fingerprint'",
            [],
            |row| row.get(0),
        )
        .context("runtime index has no chain fingerprint")?;
    let query_fingerprint = query.fingerprint();
    if let Some(cursor) = cursor {
        ensure!(
            cursor.chain_fingerprint == chain_fingerprint,
            "mapping cursor is stale for the active chain"
        );
        ensure!(
            cursor.query_fingerprint == query_fingerprint,
            "mapping cursor does not match the query filters"
        );
        ensure!(
            cursor.search_rowid.is_some() == query.search.is_some(),
            "mapping cursor search position does not match the query"
        );
    }

    let mut sql = String::from(
        "SELECT occurrence.relation_key,
                occurrence.statement_id,
                occurrence.statement,
                occurrence.record,
                occurrence.operator,
                layer.source_key,
                layer.content_hash,
                occurrence.layer_position,
                occurrence.record_ordinal,
                CASE WHEN active_relation.occurrence_id = occurrence.id THEN 1 ELSE 0 END,
                (SELECT count(*)
                 FROM occurrence AS duplicate
                 WHERE duplicate.relation_key = occurrence.relation_key),
                occurrence.evidence_fingerprint,
                coalesce((
                    SELECT group_concat(ordered_metadata.text, char(10))
                    FROM (
                        SELECT metadata.text
                        FROM metadata
                        WHERE metadata.occurrence_id = occurrence.id
                        ORDER BY metadata.ordinal
                    ) AS ordered_metadata
                ), '')
         ",
    );
    if query.search.is_some() {
        sql.push_str(", active_search.rowid ");
    } else {
        sql.push_str(", NULL ");
    }
    if query.search.is_some() {
        sql.push_str(
            "FROM active_search
             JOIN occurrence ON occurrence.id = active_search.rowid
             JOIN active_relation ON active_relation.occurrence_id = occurrence.id
             JOIN layer ON layer.position = occurrence.layer_position ",
        );
    } else if query.include_shadowed {
        sql.push_str(
            "FROM occurrence
             JOIN layer ON layer.position = occurrence.layer_position
             LEFT JOIN active_relation
               ON active_relation.relation_key = occurrence.relation_key ",
        );
    } else {
        sql.push_str(
            "FROM active_relation
             JOIN occurrence ON occurrence.id = active_relation.occurrence_id
             JOIN layer ON layer.position = occurrence.layer_position ",
        );
    }
    sql.push_str("WHERE 1 = 1 ");

    let mut parameters = Vec::<SqlValue>::new();
    if let Some(search) = &query.search {
        sql.push_str("AND active_search MATCH ? ");
        parameters.push(SqlValue::Text(search.fts_expression.clone()));
    }
    push_text_filter(
        &mut sql,
        &mut parameters,
        "occurrence.statement_id = ? ",
        query.canonical_statement_id.as_deref(),
    );
    push_text_filter(
        &mut sql,
        &mut parameters,
        "occurrence.statement_id = ? ",
        query.statement_id.as_deref(),
    );
    push_text_filter(
        &mut sql,
        &mut parameters,
        "occurrence.relation_key = ? ",
        query.relation_key.as_deref(),
    );
    if let Some(operator) = query.operator {
        sql.push_str("AND occurrence.operator = ? ");
        parameters.push(SqlValue::Text(operator.text().to_owned()));
    }
    push_text_filter(
        &mut sql,
        &mut parameters,
        "layer.source_key = ? ",
        query.source_key.as_deref(),
    );
    if let Some(endpoint) = &query.endpoint {
        sql.push_str(
            "AND EXISTS (
                SELECT 1
                FROM selection AS endpoint_filter
                WHERE endpoint_filter.occurrence_id = occurrence.id
                  AND endpoint_filter.reference = ?
             ) ",
        );
        parameters.push(SqlValue::Text(endpoint.clone()));
    }
    if query.annotation_name.is_some() || query.annotation_value.is_some() {
        sql.push_str(
            "AND EXISTS (
                SELECT 1
                FROM metadata AS annotation_filter
                WHERE annotation_filter.occurrence_id = occurrence.id ",
        );
        if let Some(name) = &query.annotation_name {
            sql.push_str("AND annotation_filter.annotation_name = ? ");
            parameters.push(SqlValue::Text(name.clone()));
        }
        if let Some(value) = &query.annotation_value {
            sql.push_str("AND annotation_filter.annotation_value = ? ");
            parameters.push(SqlValue::Text(value.clone()));
        }
        sql.push_str(") ");
    }
    if let Some(cursor) = cursor {
        if let Some(search_rowid) = cursor.search_rowid {
            sql.push_str("AND active_search.rowid > ? ");
            parameters.push(SqlValue::Integer(i64::try_from(search_rowid)?));
        } else if query.include_shadowed {
            sql.push_str(
                "AND (
                    occurrence.relation_key,
                    occurrence.layer_position,
                    occurrence.statement_id,
                    occurrence.record_ordinal
                 ) > (?, ?, ?, ?) ",
            );
            parameters.push(SqlValue::Text(cursor.relation_key.clone()));
            parameters.push(SqlValue::Integer(i64::try_from(cursor.layer_position)?));
            parameters.push(SqlValue::Text(cursor.statement_id.clone()));
            parameters.push(SqlValue::Integer(i64::try_from(cursor.record_ordinal)?));
        } else {
            sql.push_str("AND active_relation.relation_key > ? ");
            parameters.push(SqlValue::Text(cursor.relation_key.clone()));
        }
    }
    if query.search.is_some() {
        sql.push_str("ORDER BY active_search.rowid ");
    } else if query.include_shadowed {
        sql.push_str(
            "ORDER BY occurrence.relation_key,
                      occurrence.layer_position,
                      occurrence.statement_id,
                      occurrence.record_ordinal ",
        );
    } else {
        sql.push_str("ORDER BY active_relation.relation_key ");
    }
    sql.push_str("LIMIT ?");
    parameters.push(SqlValue::Integer(i64::from(query.limit) + 1));

    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement
        .query_map(params_from_iter(parameters.iter()), mapping_result_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let has_next = rows.len() > query.limit as usize;
    if has_next {
        rows.pop();
    }
    let next_cursor = has_next.then(|| {
        let row = rows.last().expect("nonempty page has a final item");
        let item = &row.item;
        MappingCursor {
            chain_fingerprint: chain_fingerprint.clone(),
            query_fingerprint,
            search_rowid: row.search_rowid,
            relation_key: item.relation_key.clone(),
            layer_position: item.layer_position,
            statement_id: item.statement_id.clone(),
            record_ordinal: item.record_ordinal,
        }
    });
    let items = rows.into_iter().map(|row| row.item).collect();
    Ok(MappingPage {
        chain_fingerprint,
        items,
        next_cursor,
    })
}

struct MappingResultRow {
    item: MappingRow,
    search_rowid: Option<u64>,
}

fn mapping_result_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MappingResultRow> {
    let operator: String = row.get(4)?;
    let operator = match operator.as_str() {
        "<=>" => Operator::Exact,
        "<~>" => Operator::Coverage,
        "=>" => Operator::Implication,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    let metadata: String = row.get(12)?;
    Ok(MappingResultRow {
        item: MappingRow {
            relation_key: row.get(0)?,
            statement_id: row.get(1)?,
            canonical_statement: row.get(2)?,
            canonical_record: row.get(3)?,
            operator,
            source_key: row.get(5)?,
            source_content_hash: row.get(6)?,
            layer_position: row.get::<_, i64>(7)? as u64,
            record_ordinal: row.get::<_, i64>(8)? as u64,
            active: row.get::<_, i64>(9)? != 0,
            occurrence_count: row.get::<_, i64>(10)? as u64,
            evidence_fingerprint: row.get(11)?,
            metadata: if metadata.is_empty() {
                Vec::new()
            } else {
                metadata.lines().map(str::to_owned).collect()
            },
        },
        search_rowid: row.get::<_, Option<i64>>(13)?.map(|value| value as u64),
    })
}

fn push_text_filter(
    sql: &mut String,
    parameters: &mut Vec<SqlValue>,
    clause: &str,
    value: Option<&str>,
) {
    if let Some(value) = value {
        sql.push_str("AND ");
        sql.push_str(clause);
        parameters.push(SqlValue::Text(value.to_owned()));
    }
}

#[derive(Clone, Debug)]
struct SearchFilter {
    normalized: String,
    fts_expression: String,
}

#[derive(Clone, Debug)]
struct NormalizedQuery {
    search: Option<SearchFilter>,
    canonical_statement: Option<String>,
    canonical_statement_id: Option<String>,
    statement_id: Option<String>,
    relation_key: Option<String>,
    operator: Option<Operator>,
    source_key: Option<String>,
    endpoint: Option<String>,
    annotation_name: Option<String>,
    annotation_value: Option<String>,
    include_shadowed: bool,
    limit: u32,
}

impl NormalizedQuery {
    fn new(query: &MappingQuery) -> Result<Self> {
        let search = normalize_search(query.search.as_deref()).map(|normalized| {
            let fts_expression = normalized
                .split_whitespace()
                .map(|term| format!("\"{}\"*", term.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" AND ");
            SearchFilter {
                normalized,
                fts_expression,
            }
        });
        if let Some(search) = &search {
            ensure!(
                search.normalized.len() <= 512,
                "mapping search exceeds the 512-byte limit"
            );
            ensure!(
                !query.include_shadowed,
                "mapping search returns active relations; query by relation key to include shadowed provenance"
            );
        }
        let canonical_statement = normalize_exact(query.canonical_statement.as_deref());
        let canonical_statement_id = canonical_statement
            .as_ref()
            .map(|text| {
                let records = parse(text).context("invalid canonical statement filter")?;
                ensure!(
                    records.len() == 1
                        && records[0].metadata.is_empty()
                        && records[0].statement.canonical() == *text,
                    "canonical statement filter is not canonical"
                );
                Ok(records[0].statement.id())
            })
            .transpose()?;
        let annotation_name = normalize_exact(query.annotation_name.as_deref());
        let annotation_value = normalize_exact(query.annotation_value.as_deref());
        ensure!(
            annotation_value.is_none() || annotation_name.is_some(),
            "annotation value filter requires an annotation name"
        );
        Ok(Self {
            search,
            canonical_statement,
            canonical_statement_id,
            statement_id: normalize_exact(query.statement_id.as_deref()),
            relation_key: normalize_exact(query.relation_key.as_deref()),
            operator: query.operator,
            source_key: normalize_exact(query.source_key.as_deref()),
            endpoint: normalize_exact(query.endpoint.as_deref()),
            annotation_name,
            annotation_value,
            include_shadowed: query.include_shadowed,
            limit: query.limit,
        })
    }

    fn fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        hash_part(&mut hasher, b"trakkin:mapping-query:v1\0");
        hash_optional(
            &mut hasher,
            self.search
                .as_ref()
                .map(|search| search.normalized.as_str()),
        );
        hash_optional(&mut hasher, self.canonical_statement.as_deref());
        hash_optional(&mut hasher, self.statement_id.as_deref());
        hash_optional(&mut hasher, self.relation_key.as_deref());
        hash_optional(&mut hasher, self.operator.map(Operator::text));
        hash_optional(&mut hasher, self.source_key.as_deref());
        hash_optional(&mut hasher, self.endpoint.as_deref());
        hash_optional(&mut hasher, self.annotation_name.as_deref());
        hash_optional(&mut hasher, self.annotation_value.as_deref());
        hash_part(&mut hasher, &[u8::from(self.include_shadowed)]);
        format!("{:x}", hasher.finalize())
    }
}

fn normalize_exact(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn normalize_search(value: Option<&str>) -> Option<String> {
    normalize_exact(value).map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn hash_optional(hasher: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            hash_part(hasher, &[1]);
            hash_part(hasher, value.as_bytes());
        }
        None => hash_part(hasher, &[0]),
    }
}

fn compile_candidate(
    layers: &[MappingLayer],
    destination: &Path,
    options: &CompileOptions,
) -> Result<CompileSummary> {
    validate_layers(layers)?;
    options.conflict_policy.validate()?;
    let policy_fingerprint = options.conflict_policy.fingerprint();
    let chain_fingerprint = chain_fingerprint(layers, &policy_fingerprint);
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
    let active_exact_claim_count: u64;
    {
        let transaction = connection.transaction()?;
        insert_conflict_policy(&transaction, &options.conflict_policy)?;
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
            let counts = compile_layer(
                &transaction,
                position as u64,
                layer,
                options.max_record_bytes,
            )?;
            occurrence_count += counts.0;
            active_relation_count += counts.1;
        }
        materialize_active_exact_claims(&transaction)?;
        materialize_active_search(&transaction)?;
        transaction.execute_batch(INDEXES)?;
        check_active_conflicts(&transaction)?;
        active_exact_claim_count =
            transaction.query_row("SELECT count(*) FROM active_exact_claim", [], |row| {
                row.get(0)
            })?;
        for (key, value) in [
            ("chain_fingerprint", chain_fingerprint.clone()),
            ("layer_count", layers.len().to_string()),
            ("occurrence_count", occurrence_count.to_string()),
            ("active_relation_count", active_relation_count.to_string()),
            (
                "shadowed_occurrence_count",
                (occurrence_count - active_relation_count).to_string(),
            ),
            (
                "active_exact_claim_count",
                active_exact_claim_count.to_string(),
            ),
            ("selector_policy", SELECTOR_POLICY_VERSION.to_owned()),
            ("conflict_policy", policy_fingerprint),
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
        active_exact_claim_count,
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
    max_record_bytes: usize,
) -> Result<(u64, u64)> {
    let file = File::open(&layer.path)
        .with_context(|| format!("opening mapping source {}", layer.path.display()))?;
    let hashing_reader = HashingReader::new(file);
    let mut reader = BufReader::with_capacity(64 * 1024, hashing_reader);
    let mut occurrence_count = 0_u64;
    let mut active_relation_count = 0_u64;
    visit_records(&mut reader, max_record_bytes, |located| {
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
        let source = unit
            .split_once("://")
            .map(|value| value.0)
            .context("resolved unit is not source-qualified")?;
        let coordinate = resolved
            .coordinates
            .as_ref()
            .and_then(|coordinates| coordinates.get(ordinal));
        transaction
            .prepare_cached(
                "INSERT INTO resolved_unit(
                    occurrence_id, side, ordinal, unit, source, extent, coordinate
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                occurrence_id,
                side,
                ordinal as u64,
                unit,
                source,
                extent,
                coordinate,
            ])?;
    }
    Ok(())
}

fn insert_conflict_policy(transaction: &Transaction<'_>, policy: &ConflictPolicy) -> Result<()> {
    for pair in &policy.exclusive_source_pairs {
        transaction
            .prepare_cached(
                "INSERT INTO exclusive_source_pair(origin_source, target_source)
                 VALUES (?1, ?2)",
            )?
            .execute(params![pair.origin_source, pair.target_source])?;
    }
    Ok(())
}

fn materialize_active_exact_claims(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        "INSERT OR IGNORE INTO active_exact_claim(
            origin_source, origin_unit, target_source, target_unit, occurrence_id
         )
         SELECT left_unit.source, left_unit.unit, right_unit.source, right_unit.unit, occurrence.id
         FROM active_relation
         JOIN occurrence ON occurrence.id = active_relation.occurrence_id
         JOIN resolved_unit AS left_unit
           ON left_unit.occurrence_id = occurrence.id AND left_unit.side = 0
         JOIN resolved_unit AS right_unit
           ON right_unit.occurrence_id = occurrence.id
          AND right_unit.side = 1
          AND right_unit.ordinal = left_unit.ordinal
         WHERE occurrence.operator = '<=>'
         UNION ALL
         SELECT right_unit.source, right_unit.unit, left_unit.source, left_unit.unit, occurrence.id
         FROM active_relation
         JOIN occurrence ON occurrence.id = active_relation.occurrence_id
         JOIN resolved_unit AS left_unit
           ON left_unit.occurrence_id = occurrence.id AND left_unit.side = 0
         JOIN resolved_unit AS right_unit
           ON right_unit.occurrence_id = occurrence.id
          AND right_unit.side = 1
          AND right_unit.ordinal = left_unit.ordinal
         WHERE occurrence.operator = '<=>';",
    )?;
    Ok(())
}

fn materialize_active_search(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        "INSERT INTO active_search(rowid, statement, endpoints, annotations)
         SELECT occurrence.id,
                occurrence.statement,
                coalesce((
                    SELECT group_concat(selection.reference, ' ')
                    FROM selection
                    WHERE selection.occurrence_id = occurrence.id
                ), ''),
                coalesce((
                    SELECT group_concat(metadata.text, ' ')
                    FROM metadata
                    WHERE metadata.occurrence_id = occurrence.id
                ), '')
         FROM active_relation
         JOIN occurrence ON occurrence.id = active_relation.occurrence_id
         ORDER BY active_relation.relation_key;",
    )?;
    Ok(())
}

fn check_active_conflicts(transaction: &Transaction<'_>) -> Result<()> {
    let conflict: Option<(String, String)> = transaction
        .query_row(
            "SELECT claim.origin_unit, claim.target_source
                         FROM active_exact_claim AS claim INDEXED BY active_exact_claim_policy
                         WHERE EXISTS (
                                 SELECT 1
                                 FROM exclusive_source_pair AS policy
                                 WHERE policy.origin_source = claim.origin_source
                                     AND policy.target_source = claim.target_source
                         )
                         AND EXISTS (
                                 SELECT 1
                                 FROM active_exact_claim AS other INDEXED BY active_exact_claim_policy
                                 WHERE other.origin_source = claim.origin_source
                                     AND other.target_source = claim.target_source
                                     AND other.origin_unit = claim.origin_unit
                                     AND other.target_unit <> claim.target_unit
                         )
                         ORDER BY claim.origin_source, claim.target_source, claim.origin_unit,
                                            claim.target_unit
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    ensure!(
        conflict.is_none(),
        "exclusive exact mapping conflict for {} toward {}",
        conflict.as_ref().map_or("", |value| value.0.as_str()),
        conflict.as_ref().map_or("", |value| value.1.as_str())
    );
    Ok(())
}

fn chain_fingerprint(layers: &[MappingLayer], policy_fingerprint: &str) -> String {
    let mut hasher = Sha256::new();
    hash_part(&mut hasher, b"trakkin:runtime-chain:v1\0");
    hash_part(&mut hasher, COMPILER_VERSION.as_bytes());
    hash_part(&mut hasher, &SCHEMA_VERSION.to_be_bytes());
    hash_part(&mut hasher, ID_VERSION.as_bytes());
    hash_part(&mut hasher, RELATION_KEY_VERSION.as_bytes());
    hash_part(&mut hasher, SELECTOR_POLICY_VERSION.as_bytes());
    hash_part(&mut hasher, policy_fingerprint.as_bytes());
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

fn source_namespace(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
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
