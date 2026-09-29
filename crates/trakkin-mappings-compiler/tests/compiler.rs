use std::path::Path;
use std::{fs, sync::Arc};
use tempfile::TempDir;
use trakkin_mappings_compiler::{
    CompileOptions, ConflictPolicy, MappingLayer, MappingQuery, MappingSide, RuntimeIndex, compile,
};
use trakkin_mappings_language::{Operator, Resolved, Resolver, Selection, digest, parse};

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
    RuntimeIndex::open(path)
        .unwrap()
        .mappings(
            &MappingQuery {
                relation_key: Some(relation_key.to_owned()),
                ..MappingQuery::default()
            },
            None,
        )
        .unwrap()
        .items
        .into_iter()
        .next()
        .unwrap()
        .source_key
}

fn evidence_rows(path: &Path) -> Vec<(String, u64, String, String)> {
    RuntimeIndex::open(path)
        .unwrap()
        .mappings(
            &MappingQuery {
                include_shadowed: true,
                limit: 250,
                ..MappingQuery::default()
            },
            None,
        )
        .unwrap()
        .items
        .into_iter()
        .map(|row| {
            (
                row.source_key,
                row.record_ordinal,
                row.relation_key,
                row.evidence_fingerprint,
            )
        })
        .collect()
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
    let rows = RuntimeIndex::open(&destination).unwrap();
    let rows = rows
        .mappings(
            &MappingQuery {
                relation_key: Some(symmetric_key),
                include_shadowed: true,
                ..MappingQuery::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(rows.items.len(), 2);
    assert_eq!(rows.items[0].source_key, "local");
    assert_eq!(rows.items[0].metadata, ["#@note high"]);
    assert_eq!(rows.items[1].source_key, "remote");
    assert_eq!(rows.items[1].metadata, ["#@note low"]);
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
    let index = RuntimeIndex::open(&destination).unwrap();

    let query = MappingQuery {
        limit: 2,
        ..MappingQuery::default()
    };
    let first = index.mappings(&query, None).unwrap();
    assert_eq!(first.chain_fingerprint, summary.chain_fingerprint);
    assert_eq!(first.items.len(), 2);
    assert!(first.items.iter().all(|item| item.active));
    let cursor = first.next_cursor.clone().unwrap();
    let second = index.mappings(&query, Some(&cursor)).unwrap();
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
    let shadowed = index
        .mappings(
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
    let search_page = index.mappings(&search_active, None).unwrap();
    assert_eq!(search_page.items.len(), 1);
    assert!(search_page.items[0].active);
    assert!(search_page.next_cursor.is_none());
    let remote_only = index
        .mappings(
            &MappingQuery {
                search: Some("remote-only".into()),
                ..MappingQuery::default()
            },
            None,
        )
        .unwrap();
    assert!(remote_only.items.is_empty());
    let error = index
        .mappings(
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
    let error = index
        .mappings(&mismatched, Some(&cursor))
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not match"), "{error}");
    let mut stale = cursor;
    stale.chain_fingerprint = "0".repeat(64);
    let error = index
        .mappings(&query, Some(&stale))
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
    let index = RuntimeIndex::open(&destination).unwrap();

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
        let page = index.mappings(&query, None).unwrap();
        assert_eq!(page.items.len(), 1, "{query:?}");
    }

    let endpoint = index
        .mappings(
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
        index
            .mappings(
                &MappingQuery {
                    search: Some(search.into()),
                    ..MappingQuery::default()
                },
                None,
            )
            .unwrap_or_else(|error| panic!("{search:?}: {error:#}"));
    }
    let error = index
        .mappings(
            &MappingQuery {
                annotation_value: Some("special  cut".into()),
                ..MappingQuery::default()
            },
            None,
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("requires an annotation name"), "{error}");
    let error = index
        .mappings(
            &MappingQuery {
                canonical_statement: Some("a://beta  => b://target/two".into()),
                ..MappingQuery::default()
            },
            None,
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("not canonical"), "{error}");
}

#[test]
fn unit_resolution_batches_exact_active_matches() {
    let temporary = TempDir::new().unwrap();
    let high = layer(
        temporary.path(),
        "high",
        "a://alpha <=> b://target/one\na://alpha => c://target/two\n",
    );
    let low = layer(temporary.path(), "low", "b://target/one <=> a://alpha\n");
    let destination = temporary.path().join("candidate.sqlite");
    let summary = compile(&[high, low], &destination, CompileOptions::default()).unwrap();
    let index = RuntimeIndex::open(&destination).unwrap();
    assert_eq!(index.chain_fingerprint(), summary.chain_fingerprint);
    index.verify_integrity().unwrap();

    let batch = index
        .resolve_units(&[
            "a://alpha".to_owned(),
            "missing://item".to_owned(),
            "a://alpha".to_owned(),
            "other://item".to_owned(),
        ])
        .unwrap();
    assert_eq!(batch.chain_fingerprint, summary.chain_fingerprint);
    assert_eq!(batch.units.len(), 3);
    assert_eq!(batch.units[0].unit_key, "a://alpha");
    assert_eq!(batch.units[0].matches.len(), 2);
    assert!(
        batch.units[0]
            .matches
            .iter()
            .all(|matched| matched.source_key == "high")
    );
    assert!(
        batch.units[0]
            .matches
            .iter()
            .all(|matched| matched.matched_side == MappingSide::Left)
    );
    assert!(batch.units[1].matches.is_empty());
    assert!(batch.units[2].matches.is_empty());
    let error = index
        .resolve_units(&["b://target/one :: episode=1".to_owned()])
        .unwrap_err()
        .to_string();
    assert!(error.contains("unit key is invalid"), "{error}");
}

#[derive(Debug)]
struct EpisodeRangeResolver;

impl Resolver for EpisodeRangeResolver {
    fn evidence_fingerprint(&self) -> String {
        "episode-range-evidence-v1".to_owned()
    }

    fn resolve(&self, selection: &Selection) -> anyhow::Result<Resolved> {
        let units = match selection.selection_key().as_str() {
            "a://show/1 :: episode=1..2,season=1" => {
                vec!["a://episode/1", "a://episode/2"]
            }
            "b://show/2 :: episode=3..4,season=1" => {
                vec!["b://episode/3", "b://episode/4"]
            }
            _ if selection.selector.is_none() => vec![selection.reference.as_str()],
            _ => anyhow::bail!("missing fixture selector evidence"),
        };
        Ok(Resolved {
            units: units.into_iter().map(str::to_owned).collect(),
            ordered: true,
            coordinates: None,
        })
    }
}

#[derive(Debug)]
struct CrossSourceResolver;

impl Resolver for CrossSourceResolver {
    fn evidence_fingerprint(&self) -> String {
        "cross-source-evidence-v1".to_owned()
    }

    fn resolve(&self, _selection: &Selection) -> anyhow::Result<Resolved> {
        Ok(Resolved {
            units: vec!["wrong://episode/1".to_owned()],
            ordered: true,
            coordinates: None,
        })
    }
}

#[derive(Debug)]
struct OrderingResolver(bool);

impl Resolver for OrderingResolver {
    fn evidence_fingerprint(&self) -> String {
        "ordering-evidence-v1".to_owned()
    }

    fn resolve(&self, selection: &Selection) -> anyhow::Result<Resolved> {
        let units = match selection.reference.as_str() {
            "a://item/1" => ["a://unit/1", "a://unit/2"],
            "b://item/1" => ["b://unit/1", "b://unit/2"],
            _ => anyhow::bail!("unexpected ordering fixture selection"),
        };
        Ok(Resolved {
            units: units.into_iter().map(str::to_owned).collect(),
            ordered: self.0,
            coordinates: Some(vec!["1".to_owned(), "2".to_owned()]),
        })
    }
}

#[test]
fn evidence_fingerprint_includes_resolved_ordering() {
    let temporary = TempDir::new().unwrap();
    let source = layer(
        temporary.path(),
        "ordering",
        "a://item/1 :: ** <~> b://item/1 :: **\n",
    );
    let ordered_path = temporary.path().join("ordered.sqlite");
    let unordered_path = temporary.path().join("unordered.sqlite");

    compile(
        std::slice::from_ref(&source),
        &ordered_path,
        CompileOptions {
            resolver: Arc::new(OrderingResolver(true)),
            ..CompileOptions::default()
        },
    )
    .unwrap();
    compile(
        &[source],
        &unordered_path,
        CompileOptions {
            resolver: Arc::new(OrderingResolver(false)),
            ..CompileOptions::default()
        },
    )
    .unwrap();

    assert_ne!(evidence_rows(&ordered_path), evidence_rows(&unordered_path));
}

#[test]
fn selector_resolution_rejects_units_owned_by_another_source() {
    let temporary = TempDir::new().unwrap();
    let source = layer(
        temporary.path(),
        "selectors",
        "a://show/1 :: season=1,episode=1 <=> b://episode/1\n",
    );
    let destination = temporary.path().join("candidate.sqlite");

    let error = compile(
        &[source],
        &destination,
        CompileOptions {
            resolver: Arc::new(CrossSourceResolver),
            ..CompileOptions::default()
        },
    )
    .unwrap_err();
    let message = format!("{error:#}");

    assert!(
        message.contains("resolver returned a unit outside the selection source"),
        "{message}"
    );
}

#[test]
fn selector_ranges_match_materialized_units_not_their_anchor() {
    let temporary = TempDir::new().unwrap();
    let source = layer(
        temporary.path(),
        "selectors",
        "a://show/1 :: season=1,episode=1..2 <=> b://show/2 :: season=1,episode=3..4\n",
    );
    let destination = temporary.path().join("candidate.sqlite");
    compile(
        &[source],
        &destination,
        CompileOptions {
            resolver: Arc::new(EpisodeRangeResolver),
            ..CompileOptions::default()
        },
    )
    .unwrap();
    let index = RuntimeIndex::open(&destination).unwrap();

    let batch = index
        .resolve_units(&["a://episode/2".to_owned(), "a://show/1".to_owned()])
        .unwrap();
    assert_eq!(batch.units[0].matches.len(), 1);
    let matched = &batch.units[0].matches[0];
    assert_eq!(matched.member_ordinal, 1);
    assert_eq!(
        matched
            .left
            .members
            .iter()
            .map(|member| member.unit_key.as_str())
            .collect::<Vec<_>>(),
        ["a://episode/1", "a://episode/2"]
    );
    assert_eq!(
        matched
            .right
            .members
            .iter()
            .map(|member| member.unit_key.as_str())
            .collect::<Vec<_>>(),
        ["b://episode/3", "b://episode/4"]
    );
    assert_eq!(matched.alignments.len(), 2);
    assert_eq!(matched.alignments[1].left_ordinal, 1);
    assert_eq!(matched.alignments[1].right_ordinal, 1);
    assert!(batch.units[1].matches.is_empty());
}

#[test]
fn materialized_relations_distinguish_weighted_alignment_from_coverage() {
    let temporary = TempDir::new().unwrap();
    let source = layer(
        temporary.path(),
        "alignment",
        "[a://one @2,a://two] => [b://one,b://two @2]\n\
         [a://one @2,a://two] <~> [c://one,c://two @2]\n",
    );
    let destination = temporary.path().join("candidate.sqlite");
    compile(&[source], &destination, CompileOptions::default()).unwrap();
    let index = RuntimeIndex::open(&destination).unwrap();

    let batch = index.resolve_units(&["a://one".to_owned()]).unwrap();
    let implication = batch.units[0]
        .matches
        .iter()
        .find(|matched| matched.operator == Operator::Implication)
        .unwrap();
    assert_eq!(implication.left.total_extent, 3);
    assert_eq!(implication.right.total_extent, 3);
    assert_eq!(implication.alignments.len(), 3);
    assert_eq!(implication.alignments[0].extent, 1);
    assert_eq!(implication.alignments[1].left_ordinal, 0);
    assert_eq!(implication.alignments[1].right_ordinal, 1);
    assert_eq!(implication.alignments[1].left_offset, 1);
    assert_eq!(implication.alignments[1].right_offset, 0);
    assert_eq!(implication.alignments[2].left_ordinal, 1);
    assert_eq!(implication.alignments[2].right_offset, 1);

    let coverage = batch.units[0]
        .matches
        .iter()
        .find(|matched| matched.operator == Operator::Coverage)
        .unwrap();
    assert!(coverage.alignments.is_empty());
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
    let index = RuntimeIndex::open(&destination).unwrap();
    let query = MappingQuery {
        search: Some("broad".into()),
        limit: 37,
        ..MappingQuery::default()
    };
    let mut cursor = None;
    let mut relation_keys = Vec::new();
    loop {
        let page = index.mappings(&query, cursor.as_ref()).unwrap();
        relation_keys.extend(page.items.into_iter().map(|item| item.relation_key));
        let Some(next_cursor) = page.next_cursor else {
            break;
        };
        assert!(next_cursor.search_position.is_some());
        cursor = Some(next_cursor);
    }
    assert_eq!(relation_keys.len(), 200);
    let mut expected_relation_keys = relation_keys.clone();
    expected_relation_keys.sort();
    expected_relation_keys.dedup();
    assert_eq!(expected_relation_keys.len(), 200);
    assert_eq!(relation_keys, expected_relation_keys);
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
}

#[test]
fn operator_filter_returns_only_matching_relations() {
    let temporary = TempDir::new().unwrap();
    let mut text = String::new();
    for index in 0..200 {
        text.push_str(&format!("a://exact/{index} <=> b://exact/{index}\n"));
    }
    text.push_str("a://directed => b://directed\n");
    let source = layer(temporary.path(), "local", &text);
    let destination = temporary.path().join("candidate.sqlite");
    compile(&[source], &destination, CompileOptions::default()).unwrap();

    let page = RuntimeIndex::open(&destination)
        .unwrap()
        .mappings(
            &MappingQuery {
                operator: Some(Operator::Implication),
                ..MappingQuery::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].operator, Operator::Implication);
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
            "selector resolution evidence is missing",
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
