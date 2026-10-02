use trakkin_mappings_language::{
    ResolvedSelection, Resolver, Selection, Selector, SemanticError, SemanticErrorKind, Value,
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
