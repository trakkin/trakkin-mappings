use std::{fs, path::Path};
use trakkin_mappings_corpus::{
    ADAPTERS, adapters::Adapters, insert, inventory, read_shard, remove, shard_path, single_record,
    statistics,
};

fn adapters() -> Adapters {
    Adapters::load(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(ADAPTERS),
    )
    .unwrap()
}

const MAPPING: &str = "com.imdb://title/tt0133093 <=> org.themoviedb://movie/603";

#[test]
fn insert_is_idempotent_and_metadata_requires_consent() {
    let root = tempfile::tempdir().unwrap();
    let adapters = adapters();
    assert_eq!(insert(root.path(), MAPPING, &adapters, false).unwrap(), 1);
    let id = single_record(MAPPING).unwrap().statement.id();
    let path = shard_path(&id).unwrap();
    assert_eq!(path, Path::new("mappings/v1/71/71c.trakkin"));
    let original = fs::read(root.path().join(&path)).unwrap();
    assert_eq!(insert(root.path(), MAPPING, &adapters, false).unwrap(), 0);
    let annotated = format!("#@note verified\n{MAPPING}");
    assert!(insert(root.path(), &annotated, &adapters, false).is_err());
    assert_eq!(fs::read(root.path().join(&path)).unwrap(), original);
    assert_eq!(insert(root.path(), &annotated, &adapters, true).unwrap(), 1);
    assert_eq!(
        read_shard(root.path(), &path, &adapters).unwrap()[0]
            .statement
            .id(),
        id
    );
    assert!(remove(root.path(), &id, &adapters).unwrap());
    assert!(!remove(root.path(), &id, &adapters).unwrap());
    assert!(inventory(root.path()).unwrap().is_empty());
}

#[test]
fn changed_shard_validation_rejects_manually_added_reverse_duplicates() {
    let root = tempfile::tempdir().unwrap();
    let adapters = adapters();
    insert(root.path(), MAPPING, &adapters, false).unwrap();
    let reversed =
        single_record("org.themoviedb://movie/603 <=> com.imdb://title/tt0133093").unwrap();
    let path = shard_path(&reversed.statement.id()).unwrap();
    fs::create_dir_all(root.path().join(&path).parent().unwrap()).unwrap();
    fs::write(root.path().join(&path), reversed.canonical()).unwrap();
    let error =
        trakkin_mappings_corpus::validate_paths(root.path(), &[path], &adapters).unwrap_err();
    assert!(error.to_string().contains("reversed duplicate"));
}

#[test]
fn invalid_batches_never_write_and_reverse_duplicates_fail() {
    let root = tempfile::tempdir().unwrap();
    let adapters = adapters();
    let invalid = format!(
        "{MAPPING}\ncom.thetvdb://series/1 :: episode=1..2,order=aired <=> org.themoviedb://tv/1"
    );
    assert!(insert(root.path(), &invalid, &adapters, false).is_err());
    assert!(inventory(root.path()).unwrap().is_empty());
    let reverse = "org.themoviedb://movie/603 <=> com.imdb://title/tt0133093";
    assert!(
        insert(
            root.path(),
            &format!("{MAPPING}\n{reverse}"),
            &adapters,
            false
        )
        .is_err()
    );
    assert!(inventory(root.path()).unwrap().is_empty());
    insert(root.path(), MAPPING, &adapters, false).unwrap();
    assert!(insert(root.path(), reverse, &adapters, false).is_err());
}

#[test]
fn anime_source_reference_shapes_are_validated() {
    let root = tempfile::tempdir().unwrap();
    let adapters = adapters();
    let mapping = "net.myanimelist://anime/5114 <=> net.anidb://anime/6107";
    assert_eq!(insert(root.path(), mapping, &adapters, false).unwrap(), 1);

    for invalid in [
        "net.myanimelist://anime/0 <=> net.anidb://anime/6107",
        "net.myanimelist://manga/5114 <=> net.anidb://anime/6107",
        "net.myanimelist://anime/5114 <=> net.anidb://anime/06107",
    ] {
        assert!(insert(root.path(), invalid, &adapters, false).is_err());
    }

    for selection in [
        "net.myanimelist://anime/5114 :: episode=1 <~> net.anidb://anime/6107",
        "net.anidb://anime/6107 :: episode=1 <~> net.myanimelist://anime/5114",
    ] {
        let error = insert(root.path(), selection, &adapters, false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("missing offline adapter evidence")
        );
    }
}

#[test]
fn shard_placement_and_order_are_deterministic() {
    let root = tempfile::tempdir().unwrap();
    let adapters = adapters();
    let text: String = (1..2000)
        .rev()
        .map(|index| format!("com.imdb://title/tt{index:07} <=> org.themoviedb://movie/{index}\n"))
        .collect();
    insert(root.path(), &text, &adapters, false).unwrap();
    let paths = inventory(root.path()).unwrap();
    for path in &paths {
        read_shard(root.path(), path, &adapters).unwrap();
    }
    assert_eq!(statistics(root.path()).unwrap().mappings, 1999);
    assert!(paths.len() > 1000);
    assert_eq!(insert(root.path(), &text, &adapters, false).unwrap(), 0);
}

#[test]
fn offline_evidence_enforces_cardinality_and_dimensions() {
    let root = tempfile::tempdir().unwrap();
    let policy = root.path().join("adapters.json");
    fs::write(&policy, r#"{"version":1,"sources":{"a":{"dimensions":["episode"],"selections":{"a://show :: episode=1..2":{"units":["a://episode/1","a://episode/2"],"ordered":true}}},"b":{"dimensions":[],"selections":{}}}}"#).unwrap();
    let adapters = Adapters::load(&policy).unwrap();
    assert!(
        insert(
            root.path(),
            "a://show :: episode=1..2 <=> b://episode/1",
            &adapters,
            false
        )
        .is_err()
    );
    insert(
        root.path(),
        "a://show :: episode=1..2 <~> b://episode/1 @2",
        &adapters,
        false,
    )
    .unwrap();
    assert!(
        insert(
            root.path(),
            "a://show :: season=1 <~> b://episode/1",
            &adapters,
            false
        )
        .is_err()
    );
}

#[test]
fn inventory_allows_registry_but_rejects_unsharded_mapping_files() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path().join(ADAPTERS);
    fs::create_dir_all(registry.parent().unwrap()).unwrap();
    fs::write(registry, "{}").unwrap();
    assert!(inventory(root.path()).unwrap().is_empty());
    fs::create_dir_all(root.path().join("mappings/manual")).unwrap();
    fs::write(root.path().join("mappings/manual/example.trakkin"), MAPPING).unwrap();
    assert!(
        inventory(root.path())
            .unwrap_err()
            .to_string()
            .contains("unexpected corpus file")
    );
}

#[test]
fn reject_path_traversal_and_wrong_shards() {
    assert!(shard_path("../x").is_err());
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("mappings/v1/00/000.trakkin");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, format!("{MAPPING}\n")).unwrap();
    assert!(
        read_shard(
            root.path(),
            Path::new("mappings/v1/00/000.trakkin"),
            &adapters()
        )
        .is_err()
    );
}
