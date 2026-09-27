use std::fs;
use std::path::Path;
use tempfile::TempDir;
use trakkin_mappings_compiler::{CompileOptions, MappingLayer, compile, open_read_only};
use trakkin_mappings_language::{digest, parse};

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
