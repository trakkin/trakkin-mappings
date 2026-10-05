use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf};
use trakkin_mappings_ingestion::{
    content_hash,
    storage::{Batch, Paimon, Storage},
};

#[test]
#[ignore = "Requires packaged Java 21 / Paimon 2.0 bridge; run with --ignored"]
fn routed_source_job_preserves_native_ids_and_coordinator_checkpoint() {
    use trakkin_mappings_ingestion::storage::RoutedStorage;
    let bridge = std::env::var_os("TRAKKIN_MAPPINGS_INGESTION_BRIDGE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("paimon/target"));
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_str().unwrap();
    let job = trakkin_mappings_ingestion::dataset::job_warehouse(
        root,
        "org.themoviedb",
        &["tv", "season"],
    )
    .unwrap();
    let open = || {
        let datasets = ["tv", "season"]
            .into_iter()
            .map(|domain| {
                let path =
                    trakkin_mappings_ingestion::dataset::warehouse(root, "org.themoviedb", domain)
                        .unwrap();
                (domain.into(), Paimon::open(&bridge, &path).unwrap())
            })
            .collect();
        RoutedStorage::new(Paimon::open(&bridge, &job).unwrap(), datasets).unwrap()
    };
    {
        let mut storage = open();
        let batch = Batch {
            records: BTreeMap::from([
                ("tv:1".into(), json!({"id":1})),
                ("season:10".into(), json!({"id":10})),
            ]),
            metadata: BTreeMap::from([(
                "season:10".into(),
                json!({"domain":"season","parent":"tv:1"}),
            )]),
            ..Batch::default()
        };
        assert_eq!(
            storage
                .commit("sync", &json!({"watermark":100}), &batch)
                .unwrap(),
            2
        );
        assert_eq!(
            storage
                .commit("sync", &json!({"watermark":200}), &batch)
                .unwrap(),
            0
        );
        assert_eq!(
            storage
                .datasets_mut()
                .get_mut("season")
                .unwrap()
                .inspect(Some("10"), 1, false)
                .unwrap()[0]["payload"]["id"],
            10
        );
    }
    let mut storage = open();
    assert_eq!(storage.checkpoint("sync").unwrap()["watermark"], 200);
    assert_eq!(
        storage.index().unwrap()["season:10"].metadata["parent"],
        "tv:1"
    );
    storage.validate().unwrap();
}

#[test]
#[ignore = "Requires packaged Java 21 / Paimon 2.0 bridge; run with --ignored"]
fn canonical_domain_tables_isolate_ids_checkpoints_and_deletions() {
    let bridge = std::env::var_os("TRAKKIN_MAPPINGS_INGESTION_BRIDGE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("paimon/target"));
    let directory = tempfile::tempdir().unwrap();
    let root = std::env::var("TRAKKIN_MAPPINGS_INGESTION_TEST_WAREHOUSE")
        .unwrap_or_else(|_| directory.path().to_str().unwrap().into());
    let movies =
        trakkin_mappings_ingestion::dataset::warehouse(&root, "com.thetvdb", "movie").unwrap();
    let series =
        trakkin_mappings_ingestion::dataset::warehouse(&root, "com.thetvdb", "series").unwrap();
    for (path, watermark) in [(&movies, 10), (&series, 20)] {
        let mut store = Paimon::open(&bridge, path).unwrap();
        store
            .commit(
                "sync",
                &json!({"watermark":watermark}),
                &Batch {
                    records: BTreeMap::from([(
                        "123".into(),
                        json!({"id":123,"watermark":watermark}),
                    )]),
                    ..Batch::default()
                },
            )
            .unwrap();
    }
    {
        let mut store = Paimon::open(&bridge, &movies).unwrap();
        store
            .commit(
                "sync",
                &json!({"watermark":30}),
                &Batch {
                    deleted: BTreeMap::from([("123".into(), json!({"reason":"not_found"}))]),
                    ..Batch::default()
                },
            )
            .unwrap();
        store.maintain().unwrap();
        store.validate().unwrap();
        assert!(store.index().unwrap()["123"].deleted);
    }
    let mut store = Paimon::open(&bridge, &series).unwrap();
    assert!(!store.index().unwrap()["123"].deleted);
    assert_eq!(store.checkpoint("sync").unwrap()["watermark"], 20);
    assert_eq!(
        store.inspect(Some("123"), 1, false).unwrap()[0]["payload"]["watermark"],
        20
    );
}

#[test]
#[ignore = "Requires packaged Java 21 / Paimon 2.0 bridge; run with --ignored"]
fn real_bridge_commits_recovers_validates_and_compacts() {
    let bridge = std::env::var_os("TRAKKIN_MAPPINGS_INGESTION_BRIDGE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("paimon/target"));
    let directory = tempfile::tempdir().unwrap();
    let warehouse = std::env::var("TRAKKIN_MAPPINGS_INGESTION_TEST_WAREHOUSE")
        .unwrap_or_else(|_| directory.path().to_str().unwrap().to_owned());
    let warehouse = warehouse.as_str();
    let record = json!({"id":1,"score":1e-8,"name":"source-native"});
    let batch = Batch {
        records: BTreeMap::from([("media:1".into(), record.clone())]),
        ..Batch::default()
    };
    let checkpoint = json!({"watermark":100});
    {
        let mut store = Paimon::open(&bridge, warehouse).unwrap();
        assert_eq!(store.commit("sync", &checkpoint, &batch).unwrap(), 1);
        let first_snapshot = store.snapshot_id().unwrap().as_i64().unwrap();
        assert_eq!(store.commit("sync", &checkpoint, &batch).unwrap(), 0);
        assert!(store.snapshot_id().unwrap().as_i64().unwrap() > first_snapshot);
        let sample = store.inspect(None, 1, false).unwrap();
        assert_eq!(sample.as_array().unwrap().len(), 1);
        assert_eq!(sample[0]["key"], "media:1");
        assert_eq!(sample[0]["payload"], record);
        assert_eq!(store.inspect(Some("media:1"), 1, false).unwrap(), sample);
        assert_eq!(
            store.inspect(Some("media:missing"), 1, false).unwrap(),
            json!([])
        );
        assert!(store.inspect(None, 0, false).is_err());
        assert_eq!(
            store.index().unwrap()["media:1"].hash,
            content_hash(&record).unwrap()
        );
        store.validate().unwrap();
    }
    {
        let mut store = Paimon::open(&bridge, warehouse).unwrap();
        assert_eq!(store.checkpoint("sync").unwrap(), checkpoint);
        assert_eq!(store.checkpoint("missing").unwrap(), json!({}));
        let deletion = Batch {
            deleted: BTreeMap::from([("media:1".into(), json!({"mergeToId":2}))]),
            ..Batch::default()
        };
        assert_eq!(
            store
                .commit("sync", &json!({"watermark":200}), &deletion)
                .unwrap(),
            1
        );
        assert!(store.index().unwrap()["media:1"].deleted);
        assert_eq!(store.inspect(None, 10, false).unwrap(), json!([]));
        assert_eq!(
            store.inspect(None, 10, true).unwrap()[0]["payload"],
            json!({"mergeToId":2})
        );
        assert_eq!(
            store.inspect(Some("media:1"), 1, false).unwrap()[0]["deleted"],
            true
        );
        store.maintain().unwrap();
        store.validate().unwrap();
    }
    {
        let mut store = Paimon::open(&bridge, warehouse).unwrap();
        assert_eq!(store.checkpoint("sync").unwrap()["watermark"], 200);
        assert_eq!(
            store
                .commit("sync", &json!({"watermark":300}), &batch)
                .unwrap(),
            1
        );
        assert!(!store.index().unwrap()["media:1"].deleted);
        store.validate().unwrap();
    }
}
