use trakkin_mappings_language::{
    CorrespondenceCapture, CorrespondenceError, CorrespondenceLimits, CorrespondenceRelation,
    MaterialGraphError, RationalPosition, ResolvedSelection, Resolver, Selection, Selector,
    SemanticError, SemanticErrorKind, Value, build_material_graph, compile_correspondence,
    evaluate, parse_record,
};

struct Fixture;
impl Resolver for Fixture {
    fn resolve(&self, selection: &Selection) -> Result<ResolvedSelection, SemanticError> {
        if selection.reference.contains("missing") {
            return Err(SemanticError::unresolved(
                "entity is absent from this snapshot",
            ));
        }
        let Some(selector) = &selection.selector else {
            return Ok(ResolvedSelection {
                units: vec![selection.reference.clone()],
                ordered: true,
                coordinates: None,
            });
        };
        let (count, ordered, recursive) = match selector {
            Selector::Recursive => (2, true, true),
            Selector::Predicates(predicates) => match predicates.get("unit").unwrap() {
                Value::Set(values) => (values.len(), false, false),
                Value::Range(_, _) => (2, true, false),
                Value::Wildcard => (3, true, false),
                Value::Scalar(_) => (1, true, false),
            },
        };
        Ok(ResolvedSelection {
            units: (0..count)
                .map(|index| format!("{}/unit/{index}", selection.reference))
                .collect(),
            ordered,
            coordinates: recursive.then(|| {
                if selection.reference.contains("different") {
                    vec!["1".into(), "3".into()]
                } else {
                    vec!["1".into(), "2".into()]
                }
            }),
        })
    }
}

fn capture() -> CorrespondenceCapture {
    CorrespondenceCapture {
        mapping_revision: "mappings-1".to_owned(),
        reference_revisions: [
            ("a".to_owned(), "a-1".to_owned()),
            ("b".to_owned(), "b-1".to_owned()),
        ]
        .into(),
    }
}

#[test]
fn material_graph_joins_only_explicitly_aligned_fragments() {
    let compile = |text| {
        compile_correspondence(
            &parse_record(text).unwrap().statement,
            &Fixture,
            capture(),
            CorrespondenceLimits::default(),
        )
        .unwrap()
    };
    let graph = build_material_graph(
        &Default::default(),
        &[compile("a://double @2 <~> [b://first,b://second]")],
        capture(),
        CorrespondenceLimits::default(),
    )
    .unwrap();
    assert_eq!(graph.atom_count(), 2);
    let parts: std::collections::BTreeSet<_> = graph
        .unit("b://first")
        .unwrap()
        .union(graph.unit("b://second").unwrap())
        .copied()
        .collect();
    assert_eq!(graph.unit("a://double"), Some(&parts));
    assert!(
        graph
            .unit("b://first")
            .unwrap()
            .is_disjoint(graph.unit("b://second").unwrap())
    );
    let graph = build_material_graph(
        &Default::default(),
        &[
            compile("a://double @2 <~> b://set :: unit={1,2}"),
            compile("a://extended => b://cut"),
        ],
        capture(),
        CorrespondenceLimits::default(),
    )
    .unwrap();
    assert_eq!(graph.groups().len(), 1);
    assert_eq!(graph.implications().len(), 1);
    assert_ne!(graph.unit("a://extended"), graph.unit("b://cut"));
}

#[test]
fn material_graph_propagates_boundaries_and_rejects_conflicting_claims() {
    let compile = |text| {
        compile_correspondence(
            &parse_record(text).unwrap().statement,
            &Fixture,
            capture(),
            CorrespondenceLimits::default(),
        )
        .unwrap()
    };
    let claims = [
        compile("a://double @2 <~> [b://first,b://second]"),
        compile("a://alias <=> b://first"),
        compile("a://alias <=> b://second"),
    ];
    assert!(matches!(
        build_material_graph(
            &Default::default(),
            &claims,
            capture(),
            CorrespondenceLimits::default()
        ),
        Err(MaterialGraphError::Conflicting { .. })
    ));
    let claims = [compile(
        "[a://1 @2,a://2 @2,a://3 @2] <~> [b://1 @3,b://2 @3]",
    )];
    let graph = build_material_graph(
        &Default::default(),
        &claims,
        capture(),
        CorrespondenceLimits::default(),
    )
    .unwrap();
    assert_eq!(graph.atom_count(), 4);
    assert_eq!(graph.unit("b://1").unwrap().len(), 2);
    assert!(matches!(
        build_material_graph(
            &Default::default(),
            &claims,
            capture(),
            CorrespondenceLimits {
                maximum_members: 2,
                ..CorrespondenceLimits::default()
            }
        ),
        Err(MaterialGraphError::LimitExceeded)
    ));
    let mut changed = capture();
    changed
        .reference_revisions
        .insert("a".to_owned(), "changed".to_owned());
    assert_eq!(
        build_material_graph(
            &Default::default(),
            &claims,
            changed,
            CorrespondenceLimits::default()
        ),
        Err(MaterialGraphError::InvalidCapture)
    );
}

#[test]
fn correspondence_compiles_exact_rational_three_to_two_coverage() {
    let statement = parse_record("[a://1 @2,a://2 @2,a://3 @2] <~> [b://1 @3,b://2 @3]")
        .unwrap()
        .statement;
    let compiled = compile_correspondence(
        &statement,
        &Fixture,
        capture(),
        CorrespondenceLimits::default(),
    )
    .unwrap();
    assert_eq!(compiled.capture(), &capture());
    let CorrespondenceRelation::OrderedCoverage(segments) = compiled.relation() else {
        panic!("expected ordered coverage");
    };
    assert_eq!(segments.len(), 4);
    assert_eq!(segments[0].left.start, RationalPosition::from_integer(0));
    assert_eq!(segments[0].left.end, RationalPosition::from_integer(1));
    assert_eq!(segments[0].right.end, RationalPosition::new(2, 3));
    assert_eq!(segments[1].left.end, RationalPosition::new(1, 2));
    assert_eq!(segments[2].left.start, RationalPosition::new(1, 2));
    assert!(compiled.left_tail().unmapped.is_empty());
    assert!(!compiled.right_tail().open);
    for scale in [2, 3, 17, 1_000_000] {
        let mut scaled = statement.clone();
        for expression in [&mut scaled.left, &mut scaled.right] {
            let trakkin_mappings_language::Expression::Composite(selections) = expression else {
                unreachable!();
            };
            for expression in selections {
                let trakkin_mappings_language::Expression::Selection(selection) = expression else {
                    unreachable!();
                };
                selection.extent =
                    std::num::NonZeroU64::new(selection.extent.get() * scale).unwrap();
            }
        }
        assert_eq!(
            compiled,
            compile_correspondence(
                &scaled,
                &Fixture,
                capture(),
                CorrespondenceLimits::default()
            )
            .unwrap()
        );
    }
}

#[test]
fn correspondence_retains_group_semantics_direction_and_unknown_tails() {
    let compile = |text| {
        compile_correspondence(
            &parse_record(text).unwrap().statement,
            &Fixture,
            capture(),
            CorrespondenceLimits::default(),
        )
        .unwrap()
    };
    assert!(
        matches!(compile("a://1 <=> b://1").relation(), CorrespondenceRelation::Exact(pairs) if pairs.len() == 1)
    );
    assert!(
        matches!(compile("a://1 @2 <~> b://2 :: unit={1,2}").relation(),
        CorrespondenceRelation::UnorderedCoverage { left, right } if left.len() == 1 && right.len() == 2)
    );
    let implication = compile("a://x :: unit=* => b://y :: unit=1..2");
    assert!(
        matches!(implication.relation(), CorrespondenceRelation::PositiveImplication(segments)
        if segments.iter().all(|segment| segment.left.reference.starts_with("a://") && segment.right.reference.starts_with("b://")))
    );
    assert!(implication.left_tail().open);
    assert_eq!(implication.left_tail().unmapped.len(), 1);
    assert!(!implication.right_tail().open);
}

#[test]
fn correspondence_bounds_fail_without_a_partial_artifact() {
    let statement = parse_record("a://x :: unit=* => b://y :: unit=1..2")
        .unwrap()
        .statement;
    for limits in [
        CorrespondenceLimits {
            maximum_members: 1,
            ..CorrespondenceLimits::default()
        },
        CorrespondenceLimits {
            maximum_work: 1,
            ..CorrespondenceLimits::default()
        },
        CorrespondenceLimits {
            maximum_depth: 0,
            ..CorrespondenceLimits::default()
        },
    ] {
        assert_eq!(
            compile_correspondence(&statement, &Fixture, capture(), limits),
            Err(CorrespondenceError::LimitExceeded)
        );
    }
    let mut missing_revision = capture();
    missing_revision.reference_revisions.remove("b");
    assert_eq!(
        compile_correspondence(
            &statement,
            &Fixture,
            missing_revision,
            CorrespondenceLimits::default()
        ),
        Err(CorrespondenceError::InvalidCapture)
    );
}

#[test]
fn correspondence_ordered_partitions_match_an_independent_integer_cell_oracle() {
    use trakkin_mappings_language::{MaterialAlignment, UnitSpan};

    for left_count in 1..=6_u64 {
        for right_count in 1..=6_u64 {
            let expression = |source, count, extent| {
                format!(
                    "[{}]",
                    (0..count)
                        .map(|index| format!("{source}://{index} @{extent}"))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            };
            let statement = parse_record(&format!(
                "{} <~> {}",
                expression("a", left_count, right_count),
                expression("b", right_count, left_count)
            ))
            .unwrap()
            .statement;
            let compiled = compile_correspondence(
                &statement,
                &Fixture,
                capture(),
                CorrespondenceLimits::default(),
            )
            .unwrap();
            let mut cells: Vec<(u64, u64, u64, u64)> = Vec::new();
            for position in 0..left_count * right_count {
                let left_index = position / right_count;
                let right_index = position / left_count;
                if let Some(last) = cells.last_mut()
                    && last.0 == left_index
                    && last.1 == right_index
                {
                    last.3 = position + 1;
                } else {
                    cells.push((left_index, right_index, position, position + 1));
                }
            }
            let expected: Vec<_> = cells
                .into_iter()
                .map(|(left, right, start, end)| MaterialAlignment {
                    left: UnitSpan {
                        reference: format!("a://{left}"),
                        start: RationalPosition::new(start - left * right_count, right_count),
                        end: RationalPosition::new(end - left * right_count, right_count),
                    },
                    right: UnitSpan {
                        reference: format!("b://{right}"),
                        start: RationalPosition::new(start - right * left_count, left_count),
                        end: RationalPosition::new(end - right * left_count, left_count),
                    },
                })
                .collect();
            assert_eq!(
                compiled.relation(),
                &CorrespondenceRelation::OrderedCoverage(expected)
            );
        }
    }
}

#[test]
fn correspondence_nested_resolution_obeys_the_hard_depth_limit() {
    let mut statement = parse_record("a://x <=> b://y").unwrap().statement;
    for _ in 0..129 {
        statement.left = trakkin_mappings_language::Expression::Composite(vec![statement.left]);
    }
    assert_eq!(
        compile_correspondence(
            &statement,
            &Fixture,
            capture(),
            CorrespondenceLimits {
                maximum_depth: usize::MAX,
                ..CorrespondenceLimits::default()
            }
        ),
        Err(CorrespondenceError::LimitExceeded)
    );
}

#[test]
fn correspondence_unordered_group_identity_is_independent_of_resolver_order() {
    struct Reversed;
    impl Resolver for Reversed {
        fn resolve(&self, selection: &Selection) -> Result<ResolvedSelection, SemanticError> {
            let mut resolved = Fixture.resolve(selection)?;
            if !resolved.ordered {
                resolved.units.reverse();
            }
            Ok(resolved)
        }
    }
    let statement = parse_record("a://x @2 <~> b://y :: unit={1,2}")
        .unwrap()
        .statement;
    assert_eq!(
        compile_correspondence(
            &statement,
            &Fixture,
            capture(),
            CorrespondenceLimits::default()
        )
        .unwrap(),
        compile_correspondence(
            &statement,
            &Reversed,
            capture(),
            CorrespondenceLimits::default()
        )
        .unwrap()
    );
}

#[test]
fn exact_correspondence_and_structural_alignment_are_shared() {
    let result = evaluate(
        &parse_record("a://x :: unit=1..2 <=> b://y :: unit=1..2")
            .unwrap()
            .statement,
        &Fixture,
    )
    .unwrap();
    assert_eq!(result.alignments.len(), 2);
    assert_eq!(result.left_unmapped_extent, 0);
    let split = evaluate(
        &parse_record("a://x @2 => b://y :: unit=1..2")
            .unwrap()
            .statement,
        &Fixture,
    )
    .unwrap();
    assert_eq!(split.alignments.len(), 2);
    assert_eq!(split.alignments[1].left_offset, 1);
}

#[test]
fn unordered_coverage_is_collective_and_never_zipped() {
    let statement = parse_record("a://x @2 <~> b://y :: unit={1,2}")
        .unwrap()
        .statement;
    let result = evaluate(&statement, &Fixture).unwrap();
    assert!(result.alignments.is_empty());
    assert_eq!(result.left.total_extent, result.right.total_extent);
    for text in [
        "a://x :: unit={1,2} <=> b://y :: unit=1..2",
        "a://x :: unit={1,2} => b://y :: unit=1..2",
    ] {
        assert!(evaluate(&parse_record(text).unwrap().statement, &Fixture).is_err());
    }
}

#[test]
fn dynamic_shared_extent_is_reported_without_changing_claim_identity() {
    let statement = parse_record("a://x :: unit=* => b://y :: unit=1..2")
        .unwrap()
        .statement;
    let id = statement.claim_id();
    let result = evaluate(&statement, &Fixture).unwrap();
    assert_eq!(result.left_unmapped_extent, 1);
    assert!(result.left.dynamic);
    assert_eq!(id, statement.claim_id());
}

#[test]
fn invalid_correspondence_is_distinct_from_unresolved_data() {
    for text in [
        "a://x <=> b://y :: unit=1..2",
        "a://x <~> b://y :: unit=1..2",
        "a://x @2 <=> b://y",
        "a://x :: ** <=> b://different :: **",
        "[a://x,a://x] <=> [b://y,b://z]",
    ] {
        assert_eq!(
            evaluate(&parse_record(text).unwrap().statement, &Fixture)
                .unwrap_err()
                .kind,
            SemanticErrorKind::Invalid
        );
    }
    let error = evaluate(
        &parse_record("a://missing <=> b://y").unwrap().statement,
        &Fixture,
    )
    .unwrap_err();
    assert_eq!(error.kind, SemanticErrorKind::Unresolved);
}

#[test]
fn recursive_coordinates_and_extent_overflow_are_validated() {
    assert!(
        evaluate(
            &parse_record("a://x :: ** <=> b://y :: **")
                .unwrap()
                .statement,
            &Fixture
        )
        .is_ok()
    );
    let statement = parse_record("[a://x @18446744073709551615,a://y] <~> b://z")
        .unwrap()
        .statement;
    assert!(
        evaluate(&statement, &Fixture)
            .unwrap_err()
            .message
            .contains("integer range")
    );
}
