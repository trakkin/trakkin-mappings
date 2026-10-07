use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf};
use trakkin_mappings_ingestion::{
    Operation, content_hash,
    storage::{Batch, PaimonBridge, Storage, Update},
};

fn bridge_path() -> PathBuf {
    std::env::var_os("TRAKKIN_MAPPINGS_INGESTION_BRIDGE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("paimon/target"))
}

#[test]
#[ignore = "Requires packaged Java 21 / Paimon 2.0 bridge; run with --ignored"]
fn routed_source_job_preserves_native_ids_and_coordinator_checkpoint() {
    use trakkin_mappings_ingestion::storage::RoutedStorage;
    let bridge_path = bridge_path();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_str().unwrap();
    let job = trakkin_mappings_ingestion::dataset::job_warehouse(
        root,
        "org.themoviedb",
        &["tv", "season"],
    )
    .unwrap();
    let open = || {
        let bridge = PaimonBridge::start(&bridge_path).unwrap();
        let datasets = ["tv", "season"]
            .into_iter()
            .map(|domain| {
                let path =
                    trakkin_mappings_ingestion::dataset::warehouse(root, "org.themoviedb", domain)
                        .unwrap();
                (domain.into(), bridge.open(&path).unwrap())
            })
            .collect();
        RoutedStorage::new(bridge.open(&job).unwrap(), datasets).unwrap()
    };
    {
        let mut storage = open();
        let batch = Batch {
            updates: BTreeMap::from([
                (
                    "tv:1".into(),
                    Update::record(&json!({"id":1}), json!({})).unwrap(),
                ),
                (
                    "season:10".into(),
                    Update::record(
                        &json!({"id":10}),
                        json!({"domain":"season","parent":"tv:1"}),
                    )
                    .unwrap(),
                ),
            ]),
        };
        storage
            .commit(Operation::Sync, &json!({"watermark":100}), &batch)
            .unwrap();
        storage
            .commit(
                Operation::Sync,
                &json!({"watermark":200}),
                &Batch::default(),
            )
            .unwrap();
        let metadata = Batch {
            updates: BTreeMap::from([(
                "season:10".into(),
                Update {
                    content: None,
                    metadata: json!({"domain":"season","parent":"tv:2"}),
                },
            )]),
        };
        storage
            .commit(Operation::Sync, &json!({"watermark":300}), &metadata)
            .unwrap();
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
    assert_eq!(
        storage.checkpoint(Operation::Sync).unwrap()["watermark"],
        300
    );
    assert_eq!(
        storage.index().unwrap()["season:10"].metadata["parent"],
        "tv:2"
    );
    for dataset in storage.datasets_mut().values_mut() {
        dataset.validate().unwrap();
    }
}

#[test]
#[ignore = "Requires packaged Java 21 / Paimon 2.0 bridge; run with --ignored"]
fn canonical_domain_tables_isolate_ids_checkpoints_and_deletions() {
    let bridge = PaimonBridge::start(&bridge_path()).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_str().unwrap();
    let movies =
        trakkin_mappings_ingestion::dataset::warehouse(root, "com.thetvdb", "movie").unwrap();
    let series =
        trakkin_mappings_ingestion::dataset::warehouse(root, "com.thetvdb", "series").unwrap();
    for (path, watermark) in [(&movies, 10), (&series, 20)] {
        let mut store = bridge.open(path).unwrap();
        store
            .commit(
                Operation::Sync,
                &json!({"watermark":watermark}),
                &Batch {
                    updates: BTreeMap::from([(
                        "123".into(),
                        Update::record(&json!({"id":123,"watermark":watermark}), json!({}))
                            .unwrap(),
                    )]),
                },
            )
            .unwrap();
    }
    {
        let mut store = bridge.open_existing(&movies).unwrap();
        store
            .commit(
                Operation::Sync,
                &json!({"watermark":30}),
                &Batch {
                    updates: BTreeMap::from([(
                        "123".into(),
                        Update::deletion(&json!({"reason":"not_found"}), json!({})).unwrap(),
                    )]),
                },
            )
            .unwrap();
        store.maintain().unwrap();
        store.validate().unwrap();
        assert!(store.index().unwrap()["123"].deleted);
    }
    let mut store = bridge.open_existing(&series).unwrap();
    assert!(!store.index().unwrap()["123"].deleted);
    assert_eq!(store.checkpoint(Operation::Sync).unwrap()["watermark"], 20);
    assert_eq!(
        store.inspect(Some("123"), 1, false).unwrap()[0]["payload"]["watermark"],
        20
    );
}

#[test]
#[ignore = "Requires packaged Java 21 / Paimon 2.0 bridge; run with --ignored"]
fn real_bridge_commits_recovers_validates_and_compacts() {
    let bridge_path = bridge_path();
    let directory = tempfile::tempdir().unwrap();
    let warehouse = directory.path().to_str().unwrap();
    let record = json!({"id":1,"score":1e-8,"name":"source-native"});
    let batch = Batch {
        updates: BTreeMap::from([(
            "media:1".into(),
            Update::record(&record, json!({})).unwrap(),
        )]),
    };
    let checkpoint = json!({"watermark":100});
    {
        let bridge = PaimonBridge::start(&bridge_path).unwrap();
        let mut store = bridge.open(warehouse).unwrap();
        store.commit(Operation::Sync, &checkpoint, &batch).unwrap();
        let first_snapshot = store.snapshot_id().unwrap().unwrap();
        store
            .commit(Operation::Sync, &checkpoint, &Batch::default())
            .unwrap();
        assert!(store.snapshot_id().unwrap().unwrap() > first_snapshot);
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
        let metadata = Batch {
            updates: BTreeMap::from([(
                "media:1".into(),
                Update {
                    content: None,
                    metadata: json!({"address":"fixture"}),
                },
            )]),
        };
        store
            .commit(Operation::Sync, &checkpoint, &metadata)
            .unwrap();
        store.validate().unwrap();
    }
    {
        let bridge = PaimonBridge::start(&bridge_path).unwrap();
        let mut store = bridge.open_existing(warehouse).unwrap();
        assert_eq!(store.checkpoint(Operation::Sync).unwrap(), checkpoint);
        assert_eq!(
            store.index().unwrap()["media:1"].metadata["address"],
            "fixture"
        );
        assert_eq!(
            store.inspect(Some("media:1"), 1, false).unwrap()[0]["payload"],
            record
        );
        assert_eq!(store.checkpoint(Operation::Reconcile).unwrap(), json!({}));
        let deletion = Batch {
            updates: BTreeMap::from([(
                "media:1".into(),
                Update::deletion(&json!({"mergeToId":2}), json!({})).unwrap(),
            )]),
        };
        store
            .commit(Operation::Sync, &json!({"watermark":200}), &deletion)
            .unwrap();
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
        let bridge = PaimonBridge::start(&bridge_path).unwrap();
        let mut store = bridge.open_existing(warehouse).unwrap();
        assert_eq!(store.checkpoint(Operation::Sync).unwrap()["watermark"], 200);
        store
            .commit(Operation::Sync, &json!({"watermark":300}), &batch)
            .unwrap();
        assert!(!store.index().unwrap()["media:1"].deleted);
        store.validate().unwrap();
    }
}
