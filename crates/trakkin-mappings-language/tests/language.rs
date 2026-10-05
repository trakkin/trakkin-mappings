use std::io::{BufReader, Cursor};
use trakkin_mappings_language::{
    LocatedRecord, Resolved, Resolver, Selection, parse, parse_selection_key, parse_unit_key,
    validate, visit_records,
};

#[test]
fn canonicalization_and_identity() {
    let first = parse("#@note original\na://x  :: z=\"word\",episode={3,1,2,1}  <~>  b://y\n")
        .unwrap()
        .remove(0);
    let canonical = "a://x :: episode={1,2,3},z=word <~> b://y";
    assert_eq!(first.statement.canonical(), canonical);
    let second = parse(canonical).unwrap().remove(0);
    assert_eq!(first.statement.id(), second.statement.id());
    assert_eq!(parse(&second.canonical()).unwrap()[0], second);
    assert_ne!(
        second.statement.id(),
        parse("b://y <~> a://x :: episode={1,2,3},z=word").unwrap()[0]
            .statement
            .id()
    );
    assert_eq!(
        second.statement.relation_key(),
        parse("b://y <~> a://x :: episode={1,2,3},z=word").unwrap()[0]
            .statement
            .relation_key()
    );
}

#[test]
fn validates_constructed_references_before_calling_resolvers() {
    #[derive(Debug)]
    struct UnreachableResolver;
    impl Resolver for UnreachableResolver {
        fn evidence_fingerprint(&self) -> String {
            "test".into()
        }

        fn validate_selection(&self, _: &Selection) -> anyhow::Result<()> {
            panic!("invalid references must not reach the resolver")
        }

        fn resolve(&self, _: &Selection) -> anyhow::Result<Resolved> {
            panic!("invalid references must not reach the resolver")
        }
    }
    for reference in [
        "missing-source",
        "://empty",
        "a://",
        "a://x :: episode=1",
        "a://x @2",
    ] {
        let mut record = parse("a://x <=> b://y").unwrap().remove(0);
        let trakkin_mappings_language::Expression::Selection(selection) =
            &mut record.statement.left
        else {
            unreachable!()
        };
        selection.reference = reference.into();
        let error = validate(&record.statement, &UnreachableResolver).unwrap_err();
        assert!(
            error.to_string().contains("invalid selection reference"),
            "{error:#}"
        );
    }
}

#[test]
fn parses_canonical_selection_and_unit_keys() {
    let key = "com.thetvdb://series/123 :: episode=2,order=aired,season=1";
    assert_eq!(parse_selection_key(key).unwrap().selection_key(), key);
    assert!(parse_unit_key(key).is_err());
    assert_eq!(
        parse_unit_key("com.thetvdb://episode/456")
            .unwrap()
            .reference,
        "com.thetvdb://episode/456"
    );

    for invalid in [
        "com.thetvdb://series/123 :: season=1,episode=2,order=aired",
        "com.thetvdb://series/123 @2",
        "[com.thetvdb://series/123]",
        "com.thetvdb://series/123 <=> org.themoviedb://tv/456",
    ] {
        assert!(parse_selection_key(invalid).is_err(), "accepted {invalid}");
    }
}

#[test]
fn mapping_identity_v1_has_a_stable_golden_hash() {
    let record = parse("com.imdb://title/tt0133093 <=> org.themoviedb://movie/603")
        .unwrap()
        .remove(0);
    assert_eq!(
        record.statement.id(),
        "71c1c687ef4d2d5eb8e21fe88a50860405f995db3c88b9b66945ef103f80aad2"
    );
}

#[test]
fn all_language_constructs_round_trip() {
    for text in [
        "com.imdb://title/tt0133093 <=> org.themoviedb://movie/603",
        "a://foo::bar => b://x",
        "[a://1 :: episode=1..12,a://2 :: episode=13..] <~> [[b://3],b://4]",
        "a://x :: ** <=> b://y :: **",
        "a://x :: season=1,episode=* <~> b://y :: episode=..12",
        "a://x :: edition=\"a,b [c]\\n\\r\\t\\\"\\\\\" => b://y",
        "a://x :: value=\"\" <=> b://y :: value=+1.5",
        "a://x :: value=\"@1\" <=> b://y :: value=\"@999\"",
        "# comment\r\n#@custom opaque payload\r\na://x <=> b://y\r\n",
        "#@custom  \na://x <=> b://y\n",
    ] {
        let records = parse(text).unwrap_or_else(|error| panic!("{text}: {error:#}"));
        let canonical: String = records.iter().map(|record| record.canonical()).collect();
        assert_eq!(parse(&canonical).unwrap(), records);
    }
    assert!(parse("").unwrap().is_empty());
}

#[test]
fn streaming_parser_visits_bounded_records_with_source_spans() {
    let text = "#@note first\r\na://x <=> b://y\r\nc://z => d://w";
    let mut records = Vec::<LocatedRecord>::new();
    visit_records(
        BufReader::with_capacity(1, Cursor::new(text.as_bytes())),
        1024,
        |record| {
            records.push(record);
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(records.len(), 2);
    assert_eq!(records[0].ordinal, 0);
    assert_eq!(records[0].span.start_line, 1);
    assert_eq!(records[0].span.end_line, 2);
    assert_eq!(records[0].span.start_byte, 0);
    assert_eq!(
        records[0].span.end_byte as usize,
        text.find("c://z").unwrap()
    );
    assert_eq!(records[0].record.metadata, ["#@note first"]);
    assert_eq!(records[1].ordinal, 1);
    assert_eq!(records[1].span.start_line, 3);
    assert_eq!(records[1].span.end_line, 3);
    assert_eq!(
        records[1].span.start_byte as usize,
        text.find("c://z").unwrap()
    );
    assert_eq!(records[1].span.end_byte as usize, text.len());
    assert_eq!(records[1].record.statement.canonical(), "c://z => d://w");
}

#[test]
fn streaming_parser_enforces_limits_and_rejects_orphan_metadata() {
    let text = "#@note first\na://x <=> b://y\n";
    let error = visit_records(Cursor::new(text.as_bytes()), text.len() - 1, |_| Ok(()))
        .unwrap_err()
        .to_string();
    assert!(error.contains("exceeds the"), "{error}");

    let error = visit_records(Cursor::new(b"# orphan\n"), 1024, |_| Ok(()))
        .unwrap_err()
        .to_string();
    assert!(error.contains("invalid mapping record"), "{error}");

    let unterminated = vec![b'a'; 1024];
    let error = visit_records(
        BufReader::with_capacity(8, Cursor::new(unterminated)),
        32,
        |_| Ok(()),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("exceeds the 32-byte limit"), "{error}");
}

#[test]
fn explicit_extent_round_trips() {
    let text = "a://x @2 <~> b://y";
    let record = parse(text).unwrap().remove(0);
    assert_eq!(record.statement.canonical(), text);
    assert_eq!(record.statement.left.selections()[0].extent.get(), 2);
}

#[test]
fn extent_ratios_are_canonicalized_statement_wide() {
    let scaled = parse("[a://x @2,a://y @4] <~> [b://x @2,b://y @4]")
        .unwrap()
        .remove(0);
    let reduced = parse("[a://x,a://y @2] <~> [b://x,b://y @2]")
        .unwrap()
        .remove(0);
    assert_eq!(scaled.statement.canonical(), reduced.statement.canonical());
    assert_eq!(scaled.statement.id(), reduced.statement.id());
    assert_eq!(parse(&scaled.canonical()).unwrap().remove(0), scaled);
    assert_eq!(
        parse("a://x @1 <~> b://y @1").unwrap()[0]
            .statement
            .canonical(),
        "a://x <~> b://y"
    );
    assert_ne!(
        reduced.statement.id(),
        parse("[a://x,a://y @3] <~> [b://x,b://y @2]").unwrap()[0]
            .statement
            .id()
    );
}

#[test]
fn malformed_input_is_rejected() {
    for text in [
        "\n",
        "# orphan",
        "#bad\na://x <=> b://y",
        "#@note\na://x <=> b://y",
        "a://x<=>b://y",
        "a://x\t<=> b://y",
        "a://x <=> b://y ",
        "a://x :: episode=1,episode=2 <=> b://y",
        "a://x :: episode={} <=> b://y",
        "a://x :: episode=.. <=> b://y",
        "a://x :: episode=1...2 <=> b://y",
        "a://x :: edition=\"\\u0041\" <=> b://y",
        "a://x @0 <~> b://y",
        "a://x @ <~> b://y",
        "a://x @-1 <~> b://y",
        "[a://x] @2 <~> b://y",
        "a://x @18446744073709551616 <~> b://y",
        "[] <=> b://y",
        "[a://x, b://y] <=> c://z",
        "a://x <=> b://y <=> c://z",
        "a://x :: episode=1,2 <=> b://y",
        "a:// <=> b://y",
        "1a://x <=> b://y",
        "a://x <=> b://y\na://z <=> b://w\r",
    ] {
        assert!(parse(text).is_err(), "accepted {text:?}");
    }
}

#[test]
fn reserved_selector_separator_stays_quoted_as_a_scalar() {
    let text = "a://x :: value=\"::\" <=> b://y";
    assert_eq!(parse(text).unwrap()[0].statement.canonical(), text);
    assert!(parse("a://x :: value=:: <=> b://y").is_err());
    let longer = "a://x :: value=::: <=> b://y";
    assert_eq!(parse(longer).unwrap()[0].statement.canonical(), longer);
}

#[test]
fn corpus_metadata_is_separate_from_identity() {
    let record = parse("#@note  test  \n#@source z\n#@reason manual-verification\n#@source a\n#@source z\na://x <=> b://y").unwrap().remove(0);
    let normalized = record.corpus_form().unwrap();
    assert_eq!(
        normalized.metadata,
        [
            "#@source a",
            "#@source z",
            "#@reason manual-verification",
            "#@note test"
        ]
    );
    assert_eq!(record.statement.id(), normalized.statement.id());
    assert!(
        parse("# human\na://x <=> b://y").unwrap()[0]
            .corpus_form()
            .is_err()
    );
}

#[derive(Debug)]
struct FixtureResolver;
impl Resolver for FixtureResolver {
    fn evidence_fingerprint(&self) -> String {
        "fixture-resolution-v1".to_owned()
    }

    fn resolve(&self, selection: &Selection) -> anyhow::Result<Resolved> {
        let text = selection.selection_key();
        let count = if text.contains("1..2")
            || text.contains("{1,2}")
            || text.contains("=*")
            || text.contains("**")
        {
            2
        } else {
            1
        };
        Ok(Resolved {
            units: (0..count)
                .map(|index| format!("{}:{index}", selection.reference))
                .collect(),
            ordered: !text.contains('{'),
            coordinates: text.contains("**").then(|| {
                vec![
                    "1".into(),
                    if text.starts_with("bad:") {
                        "3".into()
                    } else {
                        "2".into()
                    },
                ]
            }),
        })
    }
}

#[test]
fn bare_references_resolve_to_themselves_without_adapter_inference() {
    let statement = parse("a://show <=> b://show").unwrap().remove(0).statement;
    let (left, right) = validate(&statement, &FixtureResolver).unwrap();

    assert_eq!(left.units, ["a://show"]);
    assert_eq!(right.units, ["b://show"]);
}

#[test]
fn positional_and_recursive_semantics() {
    for text in [
        "a://x <=> b://y",
        "a://x :: episode=1..2 <=> b://y :: episode=1..2",
        "a://x @2 <~> b://y :: episode={1,2}",
        "a://x @2 => b://y :: episode=1..2",
        "a://x :: episode=* @2 <~> b://y",
        "a://x :: ** <=> b://y :: **",
    ] {
        validate(&parse(text).unwrap()[0].statement, &FixtureResolver).unwrap();
    }
    for text in [
        "a://x <=> b://y :: episode=1..2",
        "a://x <~> b://y :: episode={1,2}",
        "a://x <=> b://y @2",
        "a://x => b://y :: episode=1..2",
        "a://x :: episode={1,2} => b://y :: episode=1..2",
        "a://x :: ** <=> bad://y :: **",
        "a://x :: ** <=> b://y :: episode=1..2",
    ] {
        assert!(
            validate(&parse(text).unwrap()[0].statement, &FixtureResolver).is_err(),
            "{text}"
        );
    }

    let split = parse("a://x @2 <~> b://y :: episode=1..2")
        .unwrap()
        .remove(0);
    let (left, right) = validate(&split.statement, &FixtureResolver).unwrap();
    assert_eq!(left.extents, [2]);
    assert_eq!(right.extents, [1, 1]);
}
