use std::fs;
use std::path::Path;
use tempfile::TempDir;
use trakkin_mappings_compiler::{
    CompileOptions, ConflictPolicy, MappingLayer, MappingQuery, compile, list_mappings,
    open_read_only,
};
use trakkin_mappings_language::{Operator, digest, parse};

fn layer(root: &Path, source_key: &str, text: &str) -> MappingLayer {
    let path = root.join(format!("{source_key}.trakkin"));
    fs::write(&path, text).unwrap();
    MappingLayer {
        source_key: source_key.to_owned(),
        content_hash: digest(&[text.as_bytes()]),
        path,
    }
}

fn winner_source(path: &Path, relation_key: &str) -> String {
    open_read_only(path)
        .unwrap()
        .query_row(
            "SELECT layer.source_key
             FROM active_relation
             JOIN occurrence ON occurrence.id = active_relation.occurrence_id
             JOIN layer ON layer.position = occurrence.layer_position
             WHERE active_relation.relation_key = ?1",
            [relation_key],
            |row| row.get(0),
        )
        .unwrap()
}

fn evidence_rows(path: &Path) -> Vec<(String, u64, String, String)> {
    let connection = open_read_only(path).unwrap();
    connection
        .prepare(
            "SELECT layer.source_key, occurrence.record_ordinal,
                    occurrence.relation_key, occurrence.evidence_fingerprint
             FROM occurrence
             JOIN layer ON layer.position = occurrence.layer_position
             ORDER BY layer.position, occurrence.record_ordinal",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn options_with_exclusive_pair(origin_source: &str, target_source: &str) -> CompileOptions {
    CompileOptions {
        conflict_policy: ConflictPolicy::new([(origin_source, target_source)]).unwrap(),
        ..CompileOptions::default()
    }
}

#[test]
fn priority_shadows_only_equivalent_relations_and_preserves_provenance() {
    let temporary = TempDir::new().unwrap();
    let high = layer(
        temporary.path(),
        "local",
        "#@note high\na://x <=> b://y\na://x => b://y\n",
    );
    let low = layer(
        temporary.path(),
        "remote",
        "#@note low\nb://y <=> a://x\nb://y => a://x\n",
    );
    let destination = temporary.path().join("candidate.sqlite");

    let summary = compile(&[high, low], &destination, CompileOptions::default()).unwrap();
    assert_eq!(summary.occurrence_count, 4);
    assert_eq!(summary.active_relation_count, 3);
    assert_eq!(summary.shadowed_occurrence_count, 1);

    let symmetric_key = parse("a://x <=> b://y").unwrap()[0]
        .statement
        .relation_key();
    let connection = open_read_only(&destination).unwrap();
    let winner: (String, String) = connection
        .query_row(
            "SELECT layer.source_key, metadata.text
             FROM active_relation
             JOIN occurrence ON occurrence.id = active_relation.occurrence_id
             JOIN layer ON layer.position = occurrence.layer_position
             JOIN metadata ON metadata.occurrence_id = occurrence.id
             WHERE active_relation.relation_key = ?1",
            [&symmetric_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(winner, ("local".to_owned(), "#@note high".to_owned()));

    let occurrence_count: u64 = connection
        .query_row(
            "SELECT count(*) FROM occurrence WHERE relation_key = ?1",
            [&symmetric_key],
            |row| row.get(0),
        )
        .unwrap();
    let metadata_count: u64 = connection
        .query_row(
            "SELECT count(*)
             FROM metadata
             JOIN occurrence ON occurrence.id = metadata.occurrence_id
             WHERE occurrence.relation_key = ?1",
            [&symmetric_key],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(occurrence_count, 2);
    assert_eq!(metadata_count, 2);
}

#[test]
fn exact_conflicts_require_a_directional_policy_and_ignore_priority() {
    let temporary = TempDir::new().unwrap();
    let high = layer(temporary.path(), "local", "a://x <=> b://y\n");
    let low = layer(temporary.path(), "remote", "a://x <=> b://z\n");

    let unrestricted_path = temporary.path().join("unrestricted.sqlite");
    let unrestricted = compile(
        &[high.clone(), low.clone()],
        &unrestricted_path,
        CompileOptions::default(),
    )
    .unwrap();
    assert_eq!(unrestricted.active_relation_count, 2);
    assert_eq!(unrestricted.active_exact_claim_count, 4);

    let reverse_path = temporary.path().join("reverse-policy.sqlite");
    let reverse = compile(
        &[high.clone(), low.clone()],
        &reverse_path,
        options_with_exclusive_pair("b", "a"),
    )
    .unwrap();
    assert_ne!(unrestricted.chain_fingerprint, reverse.chain_fingerprint);
    let connection = open_read_only(&reverse_path).unwrap();
    let conflict_plan = connection
        .prepare(
            "EXPLAIN QUERY PLAN
             SELECT claim.origin_unit, claim.target_source
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
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    assert!(!conflict_plan.contains("TEMP B-TREE"), "{conflict_plan}");
    assert!(
        conflict_plan.contains("active_exact_claim_policy"),
        "{conflict_plan}"
    );

    let exclusive_path = temporary.path().join("exclusive.sqlite");
    let error = format!(
        "{:#}",
        compile(
            &[high, low],
            &exclusive_path,
            options_with_exclusive_pair("a", "b"),
        )
        .unwrap_err()
    );
    assert!(
        error.contains("exclusive exact mapping conflict for a://x toward b"),
        "{error}"
    );
    assert!(!exclusive_path.exists());
}

#[test]
fn shadowed_equivalents_do_not_duplicate_active_claims() {
    let temporary = TempDir::new().unwrap();
    let high = layer(temporary.path(), "local", "a://x <=> b://y\n");
    let low = layer(temporary.path(), "remote", "b://y <=> a://x\n");
    let destination = temporary.path().join("candidate.sqlite");

    let summary = compile(
        &[high, low],
        &destination,
        options_with_exclusive_pair("a", "b"),
    )
    .unwrap();
    assert_eq!(summary.occurrence_count, 2);
    assert_eq!(summary.active_relation_count, 1);
    assert_eq!(summary.active_exact_claim_count, 2);

    let connection = open_read_only(&destination).unwrap();
    let counts: (u64, u64) = connection
        .query_row(
            "SELECT count(*), count(DISTINCT occurrence_id) FROM active_exact_claim",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(counts, (2, 1));
}

#[test]
fn conflict_policy_rejects_invalid_source_namespaces() {
    let error = ConflictPolicy::new([("invalid://source", "b")])
        .unwrap_err()
        .to_string();
    assert!(error.contains("invalid exclusive origin source"), "{error}");
}

#[test]
fn mapping_queries_page_active_rows_and_expose_shadowed_provenance() {
    let temporary = TempDir::new().unwrap();
    let high = layer(
        temporary.path(),
        "local",
        "#@note special  cut\na://alpha <=> b://target/one\n\
         a://beta => b://target/two\n\
         a://gamma <~> b://target/three\n",
    );
    let low = layer(
        temporary.path(),
        "remote",
        "#@note remote-only\nb://target/one <=> a://alpha\n",
    );
    let destination = temporary.path().join("candidate.sqlite");
    let summary = compile(&[high, low], &destination, CompileOptions::default()).unwrap();
    let connection = open_read_only(&destination).unwrap();

    let active_plan = connection
        .prepare(
            "EXPLAIN QUERY PLAN
             SELECT occurrence.relation_key
             FROM active_relation
             JOIN occurrence ON occurrence.id = active_relation.occurrence_id
             ORDER BY active_relation.relation_key
             LIMIT 51",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    assert!(!active_plan.contains("TEMP B-TREE"), "{active_plan}");
    let shadowed_plan = connection
        .prepare(
            "EXPLAIN QUERY PLAN
             SELECT occurrence.relation_key
             FROM occurrence INDEXED BY occurrence_relation
             ORDER BY occurrence.relation_key, occurrence.layer_position,
                      occurrence.statement_id, occurrence.record_ordinal
             LIMIT 51",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    assert!(!shadowed_plan.contains("TEMP B-TREE"), "{shadowed_plan}");
    assert!(
        shadowed_plan.contains("occurrence_relation"),
        "{shadowed_plan}"
    );

    let query = MappingQuery {
        limit: 2,
        ..MappingQuery::default()
    };
    let first = list_mappings(&connection, &query, None).unwrap();
    assert_eq!(first.chain_fingerprint, summary.chain_fingerprint);
    assert_eq!(first.items.len(), 2);
    assert!(first.items.iter().all(|item| item.active));
    let cursor = first.next_cursor.clone().unwrap();
    let second = list_mappings(&connection, &query, Some(&cursor)).unwrap();
    assert_eq!(second.items.len(), 1);
    assert!(second.next_cursor.is_none());

    let mut relation_keys = first
        .items
        .iter()
        .chain(&second.items)
        .map(|item| item.relation_key.clone())
        .collect::<Vec<_>>();
    let mut sorted = relation_keys.clone();
    sorted.sort();
    assert_eq!(relation_keys, sorted);
    relation_keys.dedup();
    assert_eq!(relation_keys.len(), 3);
    let duplicate = first
        .items
        .iter()
        .chain(&second.items)
        .find(|item| item.canonical_statement.contains("alpha"))
        .unwrap();
    assert_eq!(duplicate.source_key, "local");
    assert_eq!(duplicate.occurrence_count, 2);
    assert_eq!(duplicate.metadata, ["#@note special  cut"]);

    let relation_key = parse("a://alpha <=> b://target/one").unwrap()[0]
        .statement
        .relation_key();
    let shadowed = list_mappings(
        &connection,
        &MappingQuery {
            relation_key: Some(relation_key),
            include_shadowed: true,
            ..MappingQuery::default()
        },
        None,
    )
    .unwrap();
    assert_eq!(shadowed.items.len(), 2);
    assert_eq!(shadowed.items[0].source_key, "local");
    assert!(shadowed.items[0].active);
    assert_eq!(shadowed.items[1].source_key, "remote");
    assert!(!shadowed.items[1].active);

    let search_active = MappingQuery {
        search: Some("special".into()),
        limit: 1,
        ..MappingQuery::default()
    };
    let search_page = list_mappings(&connection, &search_active, None).unwrap();
    assert_eq!(search_page.items.len(), 1);
    assert!(search_page.items[0].active);
    assert!(search_page.next_cursor.is_none());
    let remote_only = list_mappings(
        &connection,
        &MappingQuery {
            search: Some("remote-only".into()),
            ..MappingQuery::default()
        },
        None,
    )
    .unwrap();
    assert!(remote_only.items.is_empty());
    let indexed_documents: u64 = connection
        .query_row("SELECT count(*) FROM active_search", [], |row| row.get(0))
        .unwrap();
    assert_eq!(indexed_documents, summary.active_relation_count);
    let error = list_mappings(
        &connection,
        &MappingQuery {
            search: Some("special".into()),
            include_shadowed: true,
            ..MappingQuery::default()
        },
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("query by relation key"), "{error}");

    let mismatched = MappingQuery {
        operator: Some(Operator::Exact),
        limit: 2,
        ..MappingQuery::default()
    };
    let error = list_mappings(&connection, &mismatched, Some(&cursor))
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not match"), "{error}");
    let mut stale = cursor;
    stale.chain_fingerprint = "0".repeat(64);
    let error = list_mappings(&connection, &query, Some(&stale))
        .unwrap_err()
        .to_string();
    assert!(error.contains("stale"), "{error}");
}

#[test]
fn mapping_queries_use_fts_and_structured_filters() {
    let temporary = TempDir::new().unwrap();
    let source = layer(
        temporary.path(),
        "local",
        "#@note special  cut\na://alpha <=> b://target/one\n\
         a://beta => b://target/two\n",
    );
    let destination = temporary.path().join("candidate.sqlite");
    compile(&[source], &destination, CompileOptions::default()).unwrap();
    let connection = open_read_only(&destination).unwrap();

    for query in [
        MappingQuery {
            search: Some("special".into()),
            ..MappingQuery::default()
        },
        MappingQuery {
            search: Some("target/two".into()),
            operator: Some(Operator::Implication),
            ..MappingQuery::default()
        },
        MappingQuery {
            canonical_statement: Some("a://beta => b://target/two".into()),
            ..MappingQuery::default()
        },
        MappingQuery {
            annotation_name: Some("note".into()),
            annotation_value: Some("special  cut".into()),
            ..MappingQuery::default()
        },
    ] {
        let page = list_mappings(&connection, &query, None).unwrap();
        assert_eq!(page.items.len(), 1, "{query:?}");
    }

    let endpoint = list_mappings(
        &connection,
        &MappingQuery {
            source_key: Some("local".into()),
            endpoint: Some("b://target/two".into()),
            ..MappingQuery::default()
        },
        None,
    )
    .unwrap();
    assert_eq!(endpoint.items.len(), 1);

    for search in ["OR", "NEAR", "\"quoted\"", "*", "statement:foo"] {
        list_mappings(
            &connection,
            &MappingQuery {
                search: Some(search.into()),
                ..MappingQuery::default()
            },
            None,
        )
        .unwrap_or_else(|error| panic!("{search:?}: {error:#}"));
    }
    let error = list_mappings(
        &connection,
        &MappingQuery {
            annotation_value: Some("special  cut".into()),
            ..MappingQuery::default()
        },
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("requires an annotation name"), "{error}");
    let error = list_mappings(
        &connection,
        &MappingQuery {
            canonical_statement: Some("a://beta  => b://target/two".into()),
            ..MappingQuery::default()
        },
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("not canonical"), "{error}");

    let plan = connection
        .prepare(
            "EXPLAIN QUERY PLAN
             SELECT occurrence.id
             FROM active_search
             JOIN occurrence ON occurrence.id = active_search.rowid
             WHERE active_search MATCH '\"target\"*'",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    assert!(plan.contains("VIRTUAL TABLE INDEX"), "{plan}");
}

#[test]
fn broad_search_uses_keyset_pagination_without_sorting() {
    let temporary = TempDir::new().unwrap();
    let mut high_text = String::new();
    let mut low_text = String::new();
    for index in 0..200 {
        high_text.push_str(&format!("a://broad/{index} <=> b://target/{index}\n"));
        low_text.push_str(&format!("b://target/{index} <=> a://broad/{index}\n"));
    }
    let high = layer(temporary.path(), "high", &high_text);
    let low = layer(temporary.path(), "low", &low_text);
    let destination = temporary.path().join("candidate.sqlite");
    compile(&[high, low], &destination, CompileOptions::default()).unwrap();
    let connection = open_read_only(&destination).unwrap();
    let query = MappingQuery {
        search: Some("broad".into()),
        limit: 37,
        ..MappingQuery::default()
    };
    let mut cursor = None;
    let mut relation_keys = Vec::new();
    loop {
        let page = list_mappings(&connection, &query, cursor.as_ref()).unwrap();
        relation_keys.extend(page.items.into_iter().map(|item| item.relation_key));
        let Some(next_cursor) = page.next_cursor else {
            break;
        };
        assert!(next_cursor.search_rowid.is_some());
        cursor = Some(next_cursor);
    }
    assert_eq!(relation_keys.len(), 200);
    relation_keys.sort();
    relation_keys.dedup();
    assert_eq!(relation_keys.len(), 200);

    let plan = connection
        .prepare(
            "EXPLAIN QUERY PLAN
             SELECT occurrence.relation_key
             FROM active_search
             JOIN occurrence ON occurrence.id = active_search.rowid
             JOIN active_relation ON active_relation.occurrence_id = occurrence.id
             JOIN layer ON layer.position = occurrence.layer_position
             WHERE active_search MATCH '\"broad\"*'
             ORDER BY active_search.rowid
             LIMIT 38",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    assert!(plan.contains("VIRTUAL TABLE INDEX"), "{plan}");
    assert!(!plan.contains("TEMP B-TREE"), "{plan}");
}

#[test]
fn compilation_is_deterministic_and_layer_order_changes_the_chain() {
    let temporary = TempDir::new().unwrap();
    let first = layer(temporary.path(), "first", "a://x <=> b://y\n");
    let second = layer(temporary.path(), "second", "b://y <=> a://x\n");
    let first_path = temporary.path().join("first.sqlite");
    let second_path = temporary.path().join("second.sqlite");
    let reversed_path = temporary.path().join("reversed.sqlite");

    let first_summary = compile(
        &[first.clone(), second.clone()],
        &first_path,
        CompileOptions::default(),
    )
    .unwrap();
    let second_summary = compile(
        &[first.clone(), second.clone()],
        &second_path,
        CompileOptions::default(),
    )
    .unwrap();
    let reversed_summary =
        compile(&[second, first], &reversed_path, CompileOptions::default()).unwrap();

    assert_eq!(first_summary, second_summary);
    assert_ne!(
        first_summary.chain_fingerprint,
        reversed_summary.chain_fingerprint
    );
    let relation_key = parse("a://x <=> b://y").unwrap()[0]
        .statement
        .relation_key();
    assert_eq!(winner_source(&first_path, &relation_key), "first");
    assert_eq!(winner_source(&second_path, &relation_key), "first");
    assert_eq!(winner_source(&reversed_path, &relation_key), "second");
    assert_eq!(evidence_rows(&first_path), evidence_rows(&second_path));
    for path in [first_path, second_path, reversed_path] {
        let connection = open_read_only(&path).unwrap();
        let integrity: String = connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
    }
}

#[test]
fn operator_filter_uses_the_runtime_index() {
    let temporary = TempDir::new().unwrap();
    let mut text = String::new();
    for index in 0..200 {
        text.push_str(&format!("a://exact/{index} <=> b://exact/{index}\n"));
    }
    text.push_str("a://directed => b://directed\n");
    let source = layer(temporary.path(), "local", &text);
    let destination = temporary.path().join("candidate.sqlite");
    compile(&[source], &destination, CompileOptions::default()).unwrap();

    let connection = open_read_only(&destination).unwrap();
    let plan = connection
        .prepare(
            "EXPLAIN QUERY PLAN
             SELECT occurrence.relation_key
             FROM occurrence
             JOIN active_relation ON active_relation.occurrence_id = occurrence.id
             WHERE occurrence.operator = '=>'
             ORDER BY occurrence.relation_key",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    assert!(plan.contains("occurrence_operator"), "{plan}");
}

#[test]
fn invalid_layers_leave_no_candidate_index() {
    let temporary = TempDir::new().unwrap();
    for (name, text, expected) in [
        (
            "duplicate",
            "a://x <=> b://y\nb://y <=> a://x\n",
            "duplicate relation key",
        ),
        (
            "selector",
            "a://x :: episode=1 <=> b://y\n",
            "selectors are unsupported",
        ),
    ] {
        let source = layer(temporary.path(), name, text);
        let destination = temporary.path().join(format!("{name}.sqlite"));
        let error = format!(
            "{:#}",
            compile(&[source], &destination, CompileOptions::default()).unwrap_err()
        );
        assert!(error.contains(expected), "{error}");
        assert!(!destination.exists());
    }

    let mut changed = layer(temporary.path(), "changed", "a://x <=> b://y\n");
    changed.content_hash = "0".repeat(64);
    let destination = temporary.path().join("changed.sqlite");
    let error = format!(
        "{:#}",
        compile(&[changed], &destination, CompileOptions::default()).unwrap_err()
    );
    assert!(error.contains("content hash changed"), "{error}");
    assert!(!destination.exists());
}
