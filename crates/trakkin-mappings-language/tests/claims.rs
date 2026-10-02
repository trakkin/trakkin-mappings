use trakkin_mappings_language::{ClaimId, Decisions, parse, parse_record};

#[test]
fn decision_documents_reject_duplicate_and_contradictory_claims() {
    let claim = "a://x <=> b://y\n";
    let reverse = "#@note different metadata\nb://y <=> a://x\n";
    assert!(Decisions::parse(&(claim.to_owned() + reverse), "").is_err());
    assert!(Decisions::parse(claim, reverse).is_err());
    assert!(Decisions::parse("", "# orphan note").is_err());
}

#[test]
fn documents_canonicalize_order_and_preserve_decision_metadata() {
    let first = Decisions::parse(
        "b://2 <=> a://1\n# accepted\nb://4 <=> a://3\n",
        "#@note reviewed\nb://6 <=> a://5\n",
    )
    .unwrap();
    let second = Decisions::parse(
        "# accepted\na://3 <=> b://4\na://1 <=> b://2\n",
        "#@note reviewed\na://5 <=> b://6\n",
    )
    .unwrap();
    assert_eq!(first, second);
    let (accepted, rejected) = first.documents();
    assert!(accepted.contains("# accepted\n"));
    assert!(rejected.contains("#@note reviewed\n"));
}

#[test]
fn version_one_identity_has_a_golden_hash() {
    let record = parse_record("com.imdb://title/tt0133093 <=> org.themoviedb://movie/603").unwrap();
    assert_eq!(
        record.statement.claim_id().to_string(),
        "v1:5b85e32bbc70f29b0bc5870c5b84ab917a59256a52002b63c9386176a306e8ff"
    );
}

#[test]
fn standalone_references_reject_selectors_and_trailing_input() {
    for reference in ["com.imdb://title/tt0133093", "example://foo::bar"] {
        assert_eq!(
            trakkin_mappings_language::parse_reference(reference).unwrap(),
            reference
        );
    }
    for invalid in [
        "missing",
        "example://",
        "1example://foo",
        "example://foo :: episode=1",
        "example://foo @2",
        "example://foo\n",
        "example://foo\t",
        "example://foo <=> example://bar",
    ] {
        assert!(
            trakkin_mappings_language::parse_reference(invalid).is_err(),
            "accepted {invalid:?}"
        );
    }
}

#[test]
fn symmetric_claims_have_one_identity_and_directional_claims_do_not() {
    for operator in ["<=>", "<~>"] {
        let forward = parse_record(&format!("a://x {operator} b://y")).unwrap();
        let reverse = parse_record(&format!("b://y {operator} a://x")).unwrap();
        assert_eq!(forward, reverse);
        assert_eq!(forward.statement.claim_id(), reverse.statement.claim_id());
    }
    assert_ne!(
        parse_record("a://x => b://y").unwrap().statement.claim_id(),
        parse_record("b://y => a://x").unwrap().statement.claim_id()
    );
}

#[test]
fn canonicalization_preserves_metadata_but_excludes_it_from_identity() {
    let authored = parse_record("# human note\r\n#@source https://example.com\r\nb://y  <~>  a://x  :: z=\"word\",episode={3,1,2,1}\r\n").unwrap();
    let canonical =
        "# human note\n#@source https://example.com\na://x :: episode={1,2,3},z=word <~> b://y\n";
    assert_eq!(authored.canonical(), canonical);
    assert_eq!(parse_record(canonical).unwrap(), authored);
    let bare = parse_record("a://x :: episode={1,2,3},z=word <~> b://y").unwrap();
    assert_eq!(authored.statement.claim_id(), bare.statement.claim_id());
}

#[test]
fn extents_are_reduced_without_reordering_composites() {
    let record = parse_record("[b://y @4,b://x @2] <~> a://z @6").unwrap();
    assert_eq!(
        record.statement.canonical(),
        "[b://y @2,b://x] <~> a://z @3"
    );
    assert_eq!(parse_record(&record.canonical()).unwrap(), record);
    assert_ne!(
        record.statement.claim_id(),
        parse_record("[b://x,b://y @2] <~> a://z @3")
            .unwrap()
            .statement
            .claim_id()
    );
}

#[test]
fn claim_ids_are_explicitly_versioned_and_strictly_parsed() {
    let id = parse_record("a://x <=> b://y")
        .unwrap()
        .statement
        .claim_id();
    assert_eq!(id.to_string().parse::<ClaimId>().unwrap(), id);
    assert_eq!(id.hash().len(), 64);
    for invalid in [
        "",
        "../bad",
        &"a".repeat(64),
        &format!("v2:{}", "a".repeat(64)),
        &format!("v1:{}", "A".repeat(64)),
    ] {
        assert!(invalid.parse::<ClaimId>().is_err());
    }
}

#[test]
fn opaque_references_and_dynamic_selectors_need_no_adapter() {
    for text in [
        "a://foo::bar => b://x",
        "a://x :: ** <=> b://y :: **",
        "a://x :: season=1,episode=* <~> b://y :: episode=..12",
        "[a://1 :: episode=1..12,a://2 :: episode=13..] <~> [[b://3],b://4]",
        "a://x :: value=\"@1\",edition=\"a,b [c]\\n\\r\\t\\\"\\\\\" => b://y",
    ] {
        let record = parse_record(text).unwrap();
        assert_eq!(parse_record(&record.canonical()).unwrap(), record);
    }
    assert!(parse("").unwrap().is_empty());
}

#[test]
fn invalid_syntax_never_produces_claims() {
    for text in [
        "# orphan",
        "a://x<=>b://y",
        "a://x\t<=> b://y",
        "a://x :: episode=1,episode=2 <=> b://y",
        "a://x :: episode={} <=> b://y",
        "a://x :: episode=.. <=> b://y",
        "a://x @0 <~> b://y",
        "[] <=> b://y",
        "a://x <=> b://y <=> c://z",
        "a:// <=> b://y",
        "a://x @18446744073709551616 <=> b://y",
        "a://x :: value=\"\\u0041\" <=> b://y",
    ] {
        assert!(parse(text).is_err(), "accepted {text:?}");
    }
    assert!(parse_record("a://x <=> b://y\na://z <=> b://w").is_err());
}
