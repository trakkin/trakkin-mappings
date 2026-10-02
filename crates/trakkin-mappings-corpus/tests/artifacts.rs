use std::{fs, path::Path};
use trakkin_mappings_corpus::{
    ADAPTERS,
    adapters::Adapters,
    artifacts::{INDEX, index, release},
    insert, remove, single_record,
};

const FIRST: &str = "com.imdb://title/tt0133093 <=> org.themoviedb://movie/603";
const SECOND: &str = "com.imdb://title/tt0234215 <=> org.themoviedb://movie/604";

fn adapters() -> Adapters {
    Adapters::load(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(ADAPTERS),
    )
    .unwrap()
}

#[test]
fn incremental_matches_clean_after_insert_metadata_edit_and_delete() {
    let root = tempfile::tempdir().unwrap();
    let adapters = adapters();
    insert(root.path(), FIRST, &adapters, false).unwrap();
    let database = root.path().join(INDEX);
    assert_eq!(
        index(root.path(), &database, &adapters)
            .unwrap()
            .rebuilt_shards,
        1
    );
    assert_eq!(
        index(root.path(), &database, &adapters)
            .unwrap()
            .rebuilt_shards,
        0
    );
    insert(root.path(), SECOND, &adapters, false).unwrap();
    insert(
        root.path(),
        &format!("#@note verified\n{FIRST}"),
        &adapters,
        true,
    )
    .unwrap();
    assert_eq!(
        index(root.path(), &database, &adapters)
            .unwrap()
            .rebuilt_shards,
        2
    );
    let results =
        trakkin_mappings_corpus::index::query(&database, "com.imdb://title/tt0133093", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].record.starts_with("#@note verified"));
    remove(
        root.path(),
        &single_record(SECOND).unwrap().statement.id(),
        &adapters,
    )
    .unwrap();
    assert_eq!(
        index(root.path(), &database, &adapters)
            .unwrap()
            .removed_shards,
        1
    );
    let clean = root.path().join("clean.sqlite");
    index(root.path(), &clean, &adapters).unwrap();
    assert_eq!(
        trakkin_mappings_corpus::index::logical_hash(
            &trakkin_mappings_corpus::index::open(&database).unwrap()
        )
        .unwrap(),
        trakkin_mappings_corpus::index::logical_hash(
            &trakkin_mappings_corpus::index::open(&clean).unwrap()
        )
        .unwrap()
    );
}

#[cfg(unix)]
#[test]
fn snapshot_rejects_unrepresentable_source_paths_without_creating_output() {
    use std::os::unix::ffi::OsStringExt;
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join(std::ffi::OsString::from_vec(vec![0xff]));
    let destination = root.path().join("snapshot.sqlite");
    let error =
        trakkin_mappings_corpus::index::snapshot(&source, &destination, "test", false, "test")
            .unwrap_err();
    assert!(error.to_string().contains("source path must be UTF-8"));
    assert!(!destination.exists());
}

#[test]
fn index_returns_errors_for_constructed_invalid_annotations() {
    let root = tempfile::tempdir().unwrap();
    let connection =
        trakkin_mappings_corpus::index::open(&root.path().join("index.sqlite")).unwrap();
    let adapters = adapters();
    for metadata in ["# ordinary comment", "#@missing-value"] {
        let mut record = single_record(FIRST).unwrap();
        record.metadata.push(metadata.into());
        let resolved = trakkin_mappings_language::validate(&record.statement, &adapters).unwrap();
        let transaction = connection.unchecked_transaction().unwrap();
        transaction
            .execute("INSERT INTO shard VALUES ('test', 'hash', 'adapter')", [])
            .unwrap();
        let error = trakkin_mappings_corpus::index::insert_record(
            &transaction,
            "test",
            &record,
            &resolved,
            (false, false),
        )
        .unwrap_err();
        assert!(error.to_string().contains("annotation"));
        let mappings: u64 = transaction
            .query_row("SELECT count(*) FROM mapping", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mappings, 0);
    }
}

#[test]
fn index_rejects_invalid_references_and_cardinality_before_insertion() {
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    for invalid_reference in [
        Some("invalid"),
        Some("://identity"),
        Some("source://"),
        None,
    ] {
        let mut record = single_record(FIRST).unwrap();
        let mut resolved =
            trakkin_mappings_language::validate(&record.statement, &adapters()).unwrap();
        if let Some(reference) = invalid_reference {
            let trakkin_mappings_language::Expression::Selection(selection) =
                &mut record.statement.left
            else {
                unreachable!();
            };
            selection.reference = reference.into();
        } else {
            resolved.1.units.clear();
        }
        let error = trakkin_mappings_corpus::index::insert_record(
            &connection,
            "test",
            &record,
            &resolved,
            (false, false),
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("reference") || message.contains("cardinality"),
            "{message}"
        );
    }
}

#[test]
fn releases_are_byte_reproducible_and_queryable() {
    let root = tempfile::tempdir().unwrap();
    let adapters = adapters();
    insert(root.path(), &format!("{FIRST}\n{SECOND}"), &adapters, false).unwrap();
    let first = root.path().join("release1");
    let second = root.path().join("release2");
    let manifest = release(
        root.path(),
        &first,
        &adapters,
        "test-commit",
        false,
        1_900_000_000,
    )
    .unwrap();
    release(
        root.path(),
        &second,
        &adapters,
        "test-commit",
        false,
        1_900_000_000,
    )
    .unwrap();
    assert_eq!(manifest.mappings, 2);
    for name in [
        "manifest.json",
        "SHA256SUMS",
        "trakkin-v1.sqlite.zst",
        "trakkin-v1.trakkin.zst",
    ] {
        assert_eq!(
            fs::read(first.join(name)).unwrap(),
            fs::read(second.join(name)).unwrap(),
            "{name}"
        );
    }
    let text =
        zstd::decode_all(fs::File::open(first.join("trakkin-v1.trakkin.zst")).unwrap()).unwrap();
    assert_eq!(
        trakkin_mappings_language::parse(std::str::from_utf8(&text).unwrap())
            .unwrap()
            .len(),
        2
    );
    let sqlite = root.path().join("download.sqlite");
    fs::write(
        &sqlite,
        zstd::decode_all(fs::File::open(first.join("trakkin-v1.sqlite.zst")).unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        trakkin_mappings_corpus::index::query(&sqlite, "org.themoviedb://movie/603", 100).unwrap()
            [0]
        .statement,
        FIRST
    );
}

#[test]
fn split_assets_can_be_reassembled() {
    let root = tempfile::tempdir().unwrap();
    let adapters = adapters();
    insert(root.path(), FIRST, &adapters, false).unwrap();
    let output = root.path().join("split");
    let manifest = release(root.path(), &output, &adapters, "test", true, 100).unwrap();
    assert!(manifest.assets.len() > 2);
    for parts in manifest.streams.values() {
        let data: Vec<u8> = parts
            .iter()
            .flat_map(|name| fs::read(output.join(name)).unwrap())
            .collect();
        assert!(!zstd::decode_all(data.as_slice()).unwrap().is_empty());
    }
}

#[test]
fn repeated_bidirectional_claims_do_not_reject_valid_equivalences() {
    let root = tempfile::tempdir().unwrap();
    let adapters = adapters();
    let input = "com.imdb://title/tt0133093 <=> com.imdb://title/tt0133093\n[com.imdb://title/tt0133093,com.imdb://title/tt0234215] <=> [com.imdb://title/tt0234215,com.imdb://title/tt0133093]";
    insert(root.path(), input, &adapters, false).unwrap();
    let database = root.path().join(INDEX);
    assert_eq!(
        index(root.path(), &database, &adapters).unwrap().mappings,
        2
    );
    let connection = trakkin_mappings_corpus::index::open(&database).unwrap();
    let claims: u64 = connection
        .query_row("SELECT count(*) FROM claim", [], |row| row.get(0))
        .unwrap();
    assert_eq!(claims, 3);
}

#[test]
fn corrupted_cache_cannot_be_published() {
    let root = tempfile::tempdir().unwrap();
    let adapters = adapters();
    insert(root.path(), FIRST, &adapters, false).unwrap();
    let database = root.path().join(INDEX);
    index(root.path(), &database, &adapters).unwrap();
    let connection = trakkin_mappings_corpus::index::open(&database).unwrap();
    connection
        .execute("UPDATE mapping SET record = 'corrupted'", [])
        .unwrap();
    drop(connection);
    let output = root.path().join("release");
    let error = release(root.path(), &output, &adapters, "test", true, 1_000_000).unwrap_err();
    assert!(error.to_string().contains("differs from clean build"));
    assert!(!output.exists());
}

#[test]
fn exclusivity_conflicts_roll_back_index_updates() {
    let root = tempfile::tempdir().unwrap();
    let mut adapters = adapters();
    adapters
        .sources
        .get_mut("com.imdb")
        .unwrap()
        .exclusive_with
        .push("org.themoviedb".into());
    insert(root.path(), FIRST, &adapters, false).unwrap();
    let database = root.path().join(INDEX);
    index(root.path(), &database, &adapters).unwrap();
    insert(
        root.path(),
        "com.imdb://title/tt0133093 <=> org.themoviedb://movie/999",
        &adapters,
        false,
    )
    .unwrap();
    assert!(
        index(root.path(), &database, &adapters)
            .unwrap_err()
            .to_string()
            .contains("exclusive mapping conflict")
    );
    assert_eq!(
        trakkin_mappings_corpus::index::query(&database, "com.imdb://title/tt0133093", 100)
            .unwrap()
            .len(),
        1
    );
}
