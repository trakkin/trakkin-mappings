use anyhow::{Context, Result, ensure};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, params, params_from_iter,
    types::Value as SqlValue,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use trakkin_mappings_language::{
    Expression, ID_VERSION, IdentityResolver, Operator, RELATION_KEY_VERSION, ResolvedExpression,
    Resolver, parse, parse_unit_key, validate, visit_records,
};

pub const SCHEMA_VERSION: u32 = 2;
pub const COMPILER_VERSION: &str = "trakkin:runtime-compiler:v2\0";
pub const MAXIMUM_UNIT_RESOLUTION_BATCH_SIZE: usize = 1_000;

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
    selection_key TEXT NOT NULL,
    extent INTEGER NOT NULL,
    PRIMARY KEY(occurrence_id, side, ordinal)
) WITHOUT ROWID;
CREATE TABLE concrete_unit (
    id INTEGER PRIMARY KEY,
    unit_key TEXT NOT NULL UNIQUE,
    source TEXT NOT NULL
);
CREATE TABLE relation_side (
    occurrence_id INTEGER NOT NULL REFERENCES occurrence(id) ON DELETE CASCADE,
    side INTEGER NOT NULL,
    ordered INTEGER NOT NULL,
    total_extent INTEGER NOT NULL,
    PRIMARY KEY(occurrence_id, side)
) WITHOUT ROWID;
CREATE TABLE relation_member (
    occurrence_id INTEGER NOT NULL REFERENCES occurrence(id) ON DELETE CASCADE,
    side INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    unit_id INTEGER NOT NULL REFERENCES concrete_unit(id),
    extent INTEGER NOT NULL,
    coordinate TEXT,
    PRIMARY KEY(occurrence_id, side, ordinal),
    UNIQUE(occurrence_id, side, unit_id),
    FOREIGN KEY(occurrence_id, side)
        REFERENCES relation_side(occurrence_id, side) ON DELETE CASCADE
) WITHOUT ROWID;
CREATE TABLE alignment_segment (
    occurrence_id INTEGER NOT NULL REFERENCES occurrence(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL,
    left_ordinal INTEGER NOT NULL,
    right_ordinal INTEGER NOT NULL,
    left_offset INTEGER NOT NULL,
    right_offset INTEGER NOT NULL,
    extent INTEGER NOT NULL,
    PRIMARY KEY(occurrence_id, ordinal)
) WITHOUT ROWID;
CREATE TABLE active_relation (
    relation_key TEXT PRIMARY KEY,
    occurrence_id INTEGER NOT NULL UNIQUE REFERENCES occurrence(id)
) WITHOUT ROWID;
CREATE TABLE active_search_document (
    position INTEGER PRIMARY KEY,
    occurrence_id INTEGER NOT NULL UNIQUE REFERENCES occurrence(id)
);
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
PRAGMA user_version = 2;
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
CREATE INDEX selection_key_lookup ON selection(
    selection_key, occurrence_id, side, ordinal
);
CREATE INDEX selection_source ON selection(source, occurrence_id, side, ordinal);
CREATE INDEX relation_member_unit_lookup ON relation_member(
    unit_id, occurrence_id, side, ordinal
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
    pub resolver: Arc<dyn Resolver>,
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            max_record_bytes: 1024 * 1024,
            conflict_policy: ConflictPolicy::default(),
            resolver: Arc::new(IdentityResolver),
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
    pub search_position: Option<u64>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MappingSide {
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingRelationMember {
    pub unit_key: String,
    pub ordinal: u64,
    pub extent: u64,
    pub coordinate: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingRelationSide {
    pub expression: Expression,
    pub ordered: bool,
    pub total_extent: u64,
    pub members: Vec<MappingRelationMember>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingAlignmentSegment {
    pub left_ordinal: u64,
    pub right_ordinal: u64,
    pub left_offset: u64,
    pub right_offset: u64,
    pub extent: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingUnitMatch {
    pub relation_key: String,
    pub statement_id: String,
    pub operator: Operator,
    pub matched_side: MappingSide,
    pub member_ordinal: u64,
    pub left: MappingRelationSide,
    pub right: MappingRelationSide,
    pub alignments: Vec<MappingAlignmentSegment>,
    pub evidence_fingerprint: String,
    pub source_key: String,
    pub source_content_hash: String,
    pub layer_position: u64,
    pub record_ordinal: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingUnitResolution {
    pub unit_key: String,
    pub matches: Vec<MappingUnitMatch>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingUnitResolutionBatch {
    pub chain_fingerprint: String,
    pub units: Vec<MappingUnitResolution>,
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

pub struct RuntimeIndex {
    connection: Connection,
    chain_fingerprint: String,
}

impl RuntimeIndex {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        ensure!(
            version == SCHEMA_VERSION,
            "unsupported runtime index version; rebuild the derived index"
        );
        let chain_fingerprint = connection
            .query_row(
                "SELECT value FROM build_metadata WHERE key = 'chain_fingerprint'",
                [],
                |row| row.get(0),
            )
            .context("runtime index has no chain fingerprint")?;
        Ok(Self {
            connection,
            chain_fingerprint,
        })
    }

    pub fn chain_fingerprint(&self) -> &str {
        &self.chain_fingerprint
    }

    pub fn verify_integrity(&self) -> Result<()> {
        let result: String = self
            .connection
            .query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        ensure!(
            result == "ok",
            "runtime index integrity check failed: {result}"
        );
        Ok(())
    }

    pub fn mappings(
        &self,
        query: &MappingQuery,
        cursor: Option<&MappingCursor>,
    ) -> Result<MappingPage> {
        list_mappings(&self.connection, &self.chain_fingerprint, query, cursor)
    }

    pub fn resolve_units(&self, unit_keys: &[String]) -> Result<MappingUnitResolutionBatch> {
        resolve_mapping_units(&self.connection, &self.chain_fingerprint, unit_keys)
    }
}

fn list_mappings(
    connection: &Connection,
    chain_fingerprint: &str,
    query: &MappingQuery,
    cursor: Option<&MappingCursor>,
) -> Result<MappingPage> {
    ensure!(
        (1..=250).contains(&query.limit),
        "mapping query limit must be between 1 and 250"
    );
    let query = NormalizedQuery::new(query)?;
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
            cursor.search_position.is_some() == query.search.is_some(),
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
                         JOIN active_search_document
                             ON active_search_document.position = active_search.rowid
                         JOIN occurrence ON occurrence.id = active_search_document.occurrence_id
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
        if let Some(search_position) = cursor.search_position {
            sql.push_str("AND active_search.rowid > ? ");
            parameters.push(SqlValue::Integer(i64::try_from(search_position)?));
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
            chain_fingerprint: chain_fingerprint.to_owned(),
            query_fingerprint,
            search_position: row.search_position,
            relation_key: item.relation_key.clone(),
            layer_position: item.layer_position,
            statement_id: item.statement_id.clone(),
            record_ordinal: item.record_ordinal,
        }
    });
    let items = rows.into_iter().map(|row| row.item).collect();
    Ok(MappingPage {
        chain_fingerprint: chain_fingerprint.to_owned(),
        items,
        next_cursor,
    })
}

fn resolve_mapping_units(
    connection: &Connection,
    chain_fingerprint: &str,
    unit_keys: &[String],
) -> Result<MappingUnitResolutionBatch> {
    const QUERY_CHUNK_SIZE: usize = 250;

    ensure!(
        unit_keys.len() <= MAXIMUM_UNIT_RESOLUTION_BATCH_SIZE,
        "mapping unit resolution batch exceeds {MAXIMUM_UNIT_RESOLUTION_BATCH_SIZE} keys"
    );
    let mut unit_positions = BTreeMap::new();
    let mut units = Vec::new();
    for unit_key in unit_keys {
        ensure!(
            unit_key.len() <= 4_096,
            "mapping unit key exceeds 4096 bytes"
        );
        parse_unit_key(unit_key).context("mapping unit key is invalid")?;
        if unit_positions.contains_key(unit_key) {
            continue;
        }
        unit_positions.insert(unit_key.clone(), units.len());
        units.push(MappingUnitResolution {
            unit_key: unit_key.clone(),
            matches: Vec::new(),
        });
    }

    let unique_keys = units
        .iter()
        .map(|unit| unit.unit_key.clone())
        .collect::<Vec<_>>();
    let mut raw_matches = Vec::new();
    for chunk in unique_keys.chunks(QUERY_CHUNK_SIZE) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT concrete_unit.unit_key,
                    occurrence.id,
                    occurrence.relation_key,
                    occurrence.statement_id,
                    occurrence.operator,
                    relation_member.side,
                    relation_member.ordinal,
                    occurrence.evidence_fingerprint,
                    layer.source_key,
                    layer.content_hash,
                    occurrence.layer_position,
                    occurrence.record_ordinal
             FROM concrete_unit
             JOIN relation_member INDEXED BY relation_member_unit_lookup
               ON relation_member.unit_id = concrete_unit.id
             JOIN active_relation
               ON active_relation.occurrence_id = relation_member.occurrence_id
             JOIN occurrence ON occurrence.id = active_relation.occurrence_id
             JOIN layer ON layer.position = occurrence.layer_position
             WHERE concrete_unit.unit_key IN ({placeholders})
             ORDER BY concrete_unit.unit_key,
                      occurrence.relation_key,
                      relation_member.side,
                      relation_member.ordinal"
        );
        let mut statement = connection.prepare(&sql)?;
        raw_matches.extend(
            statement
                .query_map(params_from_iter(chunk.iter()), |row| {
                    Ok(RawMappingUnitMatch {
                        unit_key: row.get(0)?,
                        occurrence_id: row.get(1)?,
                        relation_key: row.get(2)?,
                        statement_id: row.get(3)?,
                        operator: row.get(4)?,
                        side: row.get(5)?,
                        member_ordinal: row.get::<_, i64>(6)? as u64,
                        evidence_fingerprint: row.get(7)?,
                        source_key: row.get(8)?,
                        source_content_hash: row.get(9)?,
                        layer_position: row.get::<_, i64>(10)? as u64,
                        record_ordinal: row.get::<_, i64>(11)? as u64,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }

    let occurrence_ids = raw_matches
        .iter()
        .map(|matched| matched.occurrence_id)
        .collect::<BTreeSet<_>>();
    let relations = load_materialized_relations(connection, &occurrence_ids)?;
    for row in raw_matches {
        let unit_position = unit_positions[&row.unit_key];
        let relation = relations
            .get(&row.occurrence_id)
            .context("runtime index is missing materialized relation members")?;
        units[unit_position].matches.push(row.into_match(relation)?);
    }

    Ok(MappingUnitResolutionBatch {
        chain_fingerprint: chain_fingerprint.to_owned(),
        units,
    })
}

struct RawMappingUnitMatch {
    unit_key: String,
    occurrence_id: i64,
    relation_key: String,
    statement_id: String,
    operator: String,
    side: i64,
    member_ordinal: u64,
    evidence_fingerprint: String,
    source_key: String,
    source_content_hash: String,
    layer_position: u64,
    record_ordinal: u64,
}

#[derive(Clone)]
struct MaterializedRelation {
    left: MappingRelationSide,
    right: MappingRelationSide,
    alignments: Vec<MappingAlignmentSegment>,
}

impl RawMappingUnitMatch {
    fn into_match(self, relation: &MaterializedRelation) -> Result<MappingUnitMatch> {
        let operator = match self.operator.as_str() {
            "<=>" => Operator::Exact,
            "<~>" => Operator::Coverage,
            "=>" => Operator::Implication,
            _ => anyhow::bail!("runtime index contains an invalid mapping operator"),
        };
        let matched_side = match self.side {
            0 => MappingSide::Left,
            1 => MappingSide::Right,
            _ => anyhow::bail!("runtime index contains an invalid mapping side"),
        };
        Ok(MappingUnitMatch {
            relation_key: self.relation_key,
            statement_id: self.statement_id,
            operator,
            matched_side,
            member_ordinal: self.member_ordinal,
            left: relation.left.clone(),
            right: relation.right.clone(),
            alignments: relation.alignments.clone(),
            evidence_fingerprint: self.evidence_fingerprint,
            source_key: self.source_key,
            source_content_hash: self.source_content_hash,
            layer_position: self.layer_position,
            record_ordinal: self.record_ordinal,
        })
    }
}

fn load_materialized_relations(
    connection: &Connection,
    occurrence_ids: &BTreeSet<i64>,
) -> Result<BTreeMap<i64, MaterializedRelation>> {
    const QUERY_CHUNK_SIZE: usize = 250;

    let occurrence_ids = occurrence_ids.iter().copied().collect::<Vec<_>>();
    let mut relations =
        BTreeMap::<i64, (Option<MappingRelationSide>, Option<MappingRelationSide>)>::new();
    for chunk in occurrence_ids.chunks(QUERY_CHUNK_SIZE) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT relation_side.occurrence_id, relation_side.side, relation_side.ordered,
                    relation_side.total_extent, occurrence.left_json, occurrence.right_json
             FROM relation_side
             JOIN occurrence ON occurrence.id = relation_side.occurrence_id
             WHERE relation_side.occurrence_id IN ({placeholders})
             ORDER BY relation_side.occurrence_id, relation_side.side"
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(chunk.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)? != 0,
                row.get::<_, i64>(3)? as u64,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        for row in rows {
            let (occurrence_id, side, ordered, total_extent, left_json, right_json) = row?;
            let (expression_json, target) = match side {
                0 => (left_json, 0),
                1 => (right_json, 1),
                _ => anyhow::bail!("runtime index contains an invalid mapping side"),
            };
            let materialized = MappingRelationSide {
                expression: serde_json::from_str(&expression_json)
                    .context("runtime index contains an invalid mapping expression")?,
                ordered,
                total_extent,
                members: Vec::new(),
            };
            let entry = relations.entry(occurrence_id).or_default();
            if target == 0 {
                entry.0 = Some(materialized);
            } else {
                entry.1 = Some(materialized);
            }
        }

        let sql = format!(
            "SELECT relation_member.occurrence_id, relation_member.side,
                    relation_member.ordinal, concrete_unit.unit_key,
                    relation_member.extent, relation_member.coordinate
             FROM relation_member
             JOIN concrete_unit ON concrete_unit.id = relation_member.unit_id
             WHERE relation_member.occurrence_id IN ({placeholders})
             ORDER BY relation_member.occurrence_id, relation_member.side,
                      relation_member.ordinal"
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(chunk.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                MappingRelationMember {
                    ordinal: row.get::<_, i64>(2)? as u64,
                    unit_key: row.get(3)?,
                    extent: row.get::<_, i64>(4)? as u64,
                    coordinate: row.get(5)?,
                },
            ))
        })?;
        for row in rows {
            let (occurrence_id, side, member) = row?;
            let relation = relations
                .get_mut(&occurrence_id)
                .context("runtime index is missing a materialized relation side")?;
            match side {
                0 => relation.0.as_mut(),
                1 => relation.1.as_mut(),
                _ => anyhow::bail!("runtime index contains an invalid mapping side"),
            }
            .context("runtime index is missing a materialized relation side")?
            .members
            .push(member);
        }
    }

    let mut alignments = BTreeMap::<i64, Vec<MappingAlignmentSegment>>::new();
    for chunk in occurrence_ids.chunks(QUERY_CHUNK_SIZE) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT occurrence_id, left_ordinal, right_ordinal,
                    left_offset, right_offset, extent
             FROM alignment_segment
             WHERE occurrence_id IN ({placeholders})
             ORDER BY occurrence_id, ordinal"
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(chunk.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                MappingAlignmentSegment {
                    left_ordinal: row.get::<_, i64>(1)? as u64,
                    right_ordinal: row.get::<_, i64>(2)? as u64,
                    left_offset: row.get::<_, i64>(3)? as u64,
                    right_offset: row.get::<_, i64>(4)? as u64,
                    extent: row.get::<_, i64>(5)? as u64,
                },
            ))
        })?;
        for row in rows {
            let (occurrence_id, segment) = row?;
            alignments.entry(occurrence_id).or_default().push(segment);
        }
    }

    relations
        .into_iter()
        .map(|(occurrence_id, (left, right))| {
            Ok((
                occurrence_id,
                MaterializedRelation {
                    left: left.context("runtime index is missing its left relation side")?,
                    right: right.context("runtime index is missing its right relation side")?,
                    alignments: alignments.remove(&occurrence_id).unwrap_or_default(),
                },
            ))
        })
        .collect()
}

struct MappingResultRow {
    item: MappingRow,
    search_position: Option<u64>,
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
        search_position: row.get::<_, Option<i64>>(13)?.map(|value| value as u64),
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
    let resolver_fingerprint = options.resolver.evidence_fingerprint();
    let chain_fingerprint = chain_fingerprint(layers, &policy_fingerprint, &resolver_fingerprint);
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
                options.resolver.as_ref(),
            )?;
            occurrence_count += counts.0;
            active_relation_count += counts.1;
        }
        materialize_active_search(&transaction)?;
        transaction.execute_batch(INDEXES)?;
        check_active_conflicts(&transaction, &options.conflict_policy)?;
        active_exact_claim_count = count_active_exact_claims(&transaction)?;
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
            ("resolution_evidence", resolver_fingerprint),
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
    resolver: &dyn Resolver,
) -> Result<(u64, u64)> {
    let file = File::open(&layer.path)
        .with_context(|| format!("opening mapping source {}", layer.path.display()))?;
    let hashing_reader = HashingReader::new(file);
    let mut reader = BufReader::with_capacity(64 * 1024, hashing_reader);
    let mut occurrence_count = 0_u64;
    let mut active_relation_count = 0_u64;
    visit_records(&mut reader, max_record_bytes, |located| {
        let active = insert_occurrence(transaction, position, layer, located, resolver)?;
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
    resolver: &dyn Resolver,
) -> Result<bool> {
    let statement = &located.record.statement;
    let resolved = validate(statement, resolver).with_context(|| {
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
    insert_relation_side(transaction, occurrence_id, 0, &resolved.0)?;
    insert_relation_side(transaction, occurrence_id, 1, &resolved.1)?;
    insert_alignment_segments(
        transaction,
        occurrence_id,
        statement.operator,
        &resolved.0,
        &resolved.1,
    )?;
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
                        selection_key, extent
                     ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                )?
                .execute(params![
                    occurrence_id,
                    side,
                    *ordinal,
                    path,
                    selection.reference,
                    selection.source(),
                    selection.selection_key(),
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

fn insert_relation_side(
    transaction: &Transaction<'_>,
    occurrence_id: i64,
    side: u64,
    resolved: &ResolvedExpression,
) -> Result<()> {
    let total_extent = resolved.extents.iter().try_fold(0_u64, |total, extent| {
        total
            .checked_add(*extent)
            .context("resolved relation side extent exceeds the supported integer range")
    })?;
    transaction
        .prepare_cached(
            "INSERT INTO relation_side(occurrence_id, side, ordered, total_extent)
             VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![occurrence_id, side, resolved.ordered, total_extent])?;

    for (ordinal, (unit_key, extent)) in resolved.units.iter().zip(&resolved.extents).enumerate() {
        let unit = parse_unit_key(unit_key).context("resolver returned an invalid unit key")?;
        let coordinate = resolved
            .coordinates
            .as_ref()
            .and_then(|coordinates| coordinates.get(ordinal));
        transaction
            .prepare_cached(
                "INSERT OR IGNORE INTO concrete_unit(unit_key, source) VALUES (?1, ?2)",
            )?
            .execute(params![unit_key, unit.source()])?;
        let unit_id: i64 = transaction.query_row(
            "SELECT id FROM concrete_unit WHERE unit_key = ?1",
            [unit_key],
            |row| row.get(0),
        )?;
        transaction
            .prepare_cached(
                "INSERT INTO relation_member(
                    occurrence_id, side, ordinal, unit_id, extent, coordinate
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                occurrence_id,
                side,
                ordinal as u64,
                unit_id,
                extent,
                coordinate,
            ])?;
    }
    Ok(())
}

fn insert_alignment_segments(
    transaction: &Transaction<'_>,
    occurrence_id: i64,
    operator: Operator,
    left: &ResolvedExpression,
    right: &ResolvedExpression,
) -> Result<()> {
    if operator == Operator::Coverage {
        return Ok(());
    }

    let mut left_ordinal = 0_usize;
    let mut right_ordinal = 0_usize;
    let mut left_offset = 0_u64;
    let mut right_offset = 0_u64;
    let mut segment_ordinal = 0_u64;
    while left_ordinal < left.extents.len() && right_ordinal < right.extents.len() {
        let left_remaining = left.extents[left_ordinal] - left_offset;
        let right_remaining = right.extents[right_ordinal] - right_offset;
        let extent = left_remaining.min(right_remaining);
        transaction
            .prepare_cached(
                "INSERT INTO alignment_segment(
                    occurrence_id, ordinal, left_ordinal, right_ordinal,
                    left_offset, right_offset, extent
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                occurrence_id,
                segment_ordinal,
                left_ordinal as u64,
                right_ordinal as u64,
                left_offset,
                right_offset,
                extent,
            ])?;
        segment_ordinal += 1;
        left_offset += extent;
        right_offset += extent;
        if left_offset == left.extents[left_ordinal] {
            left_ordinal += 1;
            left_offset = 0;
        }
        if right_offset == right.extents[right_ordinal] {
            right_ordinal += 1;
            right_offset = 0;
        }
    }
    Ok(())
}

fn count_active_exact_claims(transaction: &Transaction<'_>) -> Result<u64> {
    let pair_count: u64 = transaction.query_row(
        "SELECT count(*)
         FROM active_relation
         JOIN occurrence ON occurrence.id = active_relation.occurrence_id
         JOIN relation_member
           ON relation_member.occurrence_id = occurrence.id
          AND relation_member.side = 0
         WHERE occurrence.operator = '<=>'",
        [],
        |row| row.get(0),
    )?;
    pair_count
        .checked_mul(2)
        .context("active exact claim count exceeds the supported integer range")
}

fn materialize_active_search(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        "INSERT INTO active_search_document(position, occurrence_id)
         SELECT row_number() OVER (ORDER BY active_relation.relation_key), occurrence.id
         FROM active_relation
         JOIN occurrence ON occurrence.id = active_relation.occurrence_id;
         INSERT INTO active_search(rowid, statement, endpoints, annotations)
         SELECT active_search_document.position,
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
         FROM active_search_document
         JOIN occurrence ON occurrence.id = active_search_document.occurrence_id
         ORDER BY active_search_document.position;",
    )?;
    Ok(())
}

fn check_active_conflicts(transaction: &Transaction<'_>, policy: &ConflictPolicy) -> Result<()> {
    for pair in &policy.exclusive_source_pairs {
        let conflict: Option<String> = transaction
            .query_row(
                "WITH exact_claim(origin_source, origin_unit_key, target_source, target_unit_key) AS (
                     SELECT left_unit.source, left_unit.unit_key,
                            right_unit.source, right_unit.unit_key
                     FROM active_relation
                     JOIN occurrence ON occurrence.id = active_relation.occurrence_id
                     JOIN relation_member AS left_member
                       ON left_member.occurrence_id = occurrence.id AND left_member.side = 0
                     JOIN concrete_unit AS left_unit ON left_unit.id = left_member.unit_id
                     JOIN relation_member AS right_member
                       ON right_member.occurrence_id = occurrence.id
                      AND right_member.side = 1
                      AND right_member.ordinal = left_member.ordinal
                     JOIN concrete_unit AS right_unit ON right_unit.id = right_member.unit_id
                     WHERE occurrence.operator = '<=>'
                     UNION ALL
                     SELECT right_unit.source, right_unit.unit_key,
                            left_unit.source, left_unit.unit_key
                     FROM active_relation
                     JOIN occurrence ON occurrence.id = active_relation.occurrence_id
                     JOIN relation_member AS left_member
                       ON left_member.occurrence_id = occurrence.id AND left_member.side = 0
                     JOIN concrete_unit AS left_unit ON left_unit.id = left_member.unit_id
                     JOIN relation_member AS right_member
                       ON right_member.occurrence_id = occurrence.id
                      AND right_member.side = 1
                      AND right_member.ordinal = left_member.ordinal
                     JOIN concrete_unit AS right_unit ON right_unit.id = right_member.unit_id
                     WHERE occurrence.operator = '<=>'
                 )
                 SELECT origin_unit_key
                 FROM exact_claim
                 WHERE origin_source = ?1 AND target_source = ?2
                 GROUP BY origin_unit_key
                 HAVING min(target_unit_key) <> max(target_unit_key)
                 ORDER BY origin_unit_key
                 LIMIT 1",
                params![pair.origin_source, pair.target_source],
                |row| row.get(0),
            )
            .optional()?;
        ensure!(
            conflict.is_none(),
            "exclusive exact mapping conflict for {} toward {}",
            conflict.as_deref().unwrap_or(""),
            pair.target_source
        );
    }
    Ok(())
}

fn chain_fingerprint(
    layers: &[MappingLayer],
    policy_fingerprint: &str,
    resolver_fingerprint: &str,
) -> String {
    let mut hasher = Sha256::new();
    hash_part(&mut hasher, b"trakkin:runtime-chain:v1\0");
    hash_part(&mut hasher, COMPILER_VERSION.as_bytes());
    hash_part(&mut hasher, &SCHEMA_VERSION.to_be_bytes());
    hash_part(&mut hasher, ID_VERSION.as_bytes());
    hash_part(&mut hasher, RELATION_KEY_VERSION.as_bytes());
    hash_part(&mut hasher, resolver_fingerprint.as_bytes());
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
        hash_part(&mut hasher, &[u8::from(expression.ordered)]);
        for (ordinal, (unit, extent)) in
            expression.units.iter().zip(&expression.extents).enumerate()
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
