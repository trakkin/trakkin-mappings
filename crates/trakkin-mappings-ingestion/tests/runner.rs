use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use trakkin_mappings_ingestion::{
    Change, Operation, Page, Provider, Report, content_hash,
    storage::{Batch, Entry, Storage},
};

fn run(
    provider: &mut impl Provider,
    storage: &mut impl Storage,
    operation: Operation,
    batch_size: usize,
    now: i64,
) -> Result<Report> {
    trakkin_mappings_ingestion::run(provider, storage, operation, batch_size, now, |_, _| {})
}

#[derive(Default)]
struct Memory {
    entries: BTreeMap<String, Entry>,
    checkpoints: BTreeMap<String, Value>,
    records: BTreeMap<String, Value>,
    tombstones: BTreeMap<String, Value>,
    fail_after_commit: bool,
    fail_before_commit: bool,
    commits: Vec<(Value, usize)>,
    metadata_writes: usize,
}

impl Storage for Memory {
    fn index(&mut self) -> Result<BTreeMap<String, Entry>> {
        Ok(self.entries.clone())
    }
    fn checkpoint(&mut self, operation: &str) -> Result<Value> {
        Ok(self
            .checkpoints
            .get(operation)
            .cloned()
            .unwrap_or(json!({})))
    }
    fn commit(&mut self, operation: &str, checkpoint: &Value, batch: &Batch) -> Result<u64> {
        if self.fail_before_commit {
            self.fail_before_commit = false;
            bail!("Commit rejected before persistence");
        }
        self.metadata_writes += batch.metadata.len();
        self.commits.push((
            checkpoint.clone(),
            batch.records.len() + batch.deleted.len(),
        ));
        for (key, record) in &batch.records {
            self.entries.insert(
                key.clone(),
                Entry {
                    hash: content_hash(record)?,
                    deleted: false,
                    metadata: batch.metadata.get(key).cloned().unwrap_or(json!({})),
                },
            );
            self.records.insert(key.clone(), record.clone());
            self.tombstones.remove(key);
        }
        for (key, metadata) in &batch.deleted {
            self.entries.insert(
                key.clone(),
                Entry {
                    hash: content_hash(metadata)?,
                    deleted: true,
                    metadata: batch.metadata.get(key).cloned().unwrap_or(json!({})),
                },
            );
            self.records.remove(key);
            self.tombstones.insert(key.clone(), metadata.clone());
        }
        self.checkpoints
            .insert(operation.into(), checkpoint.clone());
        for (key, metadata) in &batch.metadata {
            if let Some(entry) = self.entries.get_mut(key) {
                entry.metadata = metadata.clone();
            }
        }
        if self.fail_after_commit && !batch.records.is_empty() {
            self.fail_after_commit = false;
            bail!("Lost acknowledgement after durable commit");
        }
        Ok((batch.records.len() + batch.deleted.len()) as u64)
    }
    fn validate(&mut self) -> Result<()> {
        Ok(())
    }
    fn maintain(&mut self) -> Result<()> {
        Ok(())
    }
}

struct Fixture {
    pages: Vec<Vec<Change>>,
    records: BTreeMap<String, Value>,
    fetched: Vec<String>,
    fail_page: Option<usize>,
    descriptors: BTreeMap<String, Value>,
}

impl Fixture {
    fn new(keys: &[&str]) -> Self {
        Self {
            pages: vec![
                keys.iter()
                    .map(|key| Change::Dirty((*key).into()))
                    .collect(),
            ],
            records: keys
                .iter()
                .map(|key| ((*key).into(), json!({"id":key})))
                .collect(),
            fetched: vec![],
            fail_page: None,
            descriptors: BTreeMap::new(),
        }
    }
    fn page(&self, cursor: &Value) -> Result<Page> {
        let page = cursor["page"].as_u64().unwrap_or(0) as usize;
        if self.fail_page == Some(page) {
            bail!("Interrupted enumeration");
        }
        Ok(Page {
            changes: self.pages[page].clone(),
            next: (page + 1 < self.pages.len()).then(|| json!({"page":page+1})),
        })
    }
}

impl Provider for Fixture {
    fn source(&self) -> Option<&'static str> {
        Some("com.thetvdb")
    }
    fn metadata(&self, key: &str) -> Value {
        self.descriptors.get(key).cloned().unwrap_or(json!({}))
    }
    fn enumerate(&mut self, cursor: &Value, _started: i64) -> Result<Page> {
        self.page(cursor)
    }
    fn discover_changes(&mut self, _watermark: i64, cursor: &Value, _until: i64) -> Result<Page> {
        self.page(cursor)
    }
    fn fetch(&mut self, key: &str) -> Result<Option<Value>> {
        self.fetched.push(key.into());
        Ok(self.records.get(key).cloned())
    }
}

#[test]
fn routed_job_replays_partial_destination_commits_without_advancing_cursor() {
    use trakkin_mappings_ingestion::storage::RoutedStorage;
    let mut storage = RoutedStorage::new(
        Memory::default(),
        BTreeMap::from([
            ("episode".into(), Memory::default()),
            (
                "season".into(),
                Memory {
                    fail_after_commit: true,
                    ..Memory::default()
                },
            ),
            ("tv".into(), Memory::default()),
        ]),
    )
    .unwrap();
    let mut provider = Fixture::new(&["tv:1", "season:10", "episode:100"]);
    provider.pages.push(vec![]);
    assert!(run(&mut provider, &mut storage, Operation::Bootstrap, 100, 100).is_err());
    assert!(storage.checkpoint("bootstrap").unwrap()["cursor"].is_null());
    assert!(storage.datasets_mut()["tv"].records.is_empty());
    assert_eq!(storage.datasets_mut()["season"].records.len(), 1);
    run(&mut provider, &mut storage, Operation::Bootstrap, 100, 200).unwrap();
    assert_eq!(storage.index().unwrap().len(), 3);
    assert_eq!(storage.datasets_mut()["tv"].records["1"]["id"], "tv:1");
    assert_eq!(storage.checkpoint("sync").unwrap()["watermark"], 100);
}

#[test]
fn coordinator_failure_keeps_job_checkpoint_behind_durable_domain_writes() {
    use trakkin_mappings_ingestion::storage::RoutedStorage;
    let coordinator = Memory {
        checkpoints: BTreeMap::from([("sync".into(), json!({"watermark":10}))]),
        fail_before_commit: true,
        ..Memory::default()
    };
    let mut storage = RoutedStorage::new(
        coordinator,
        BTreeMap::from([
            ("tv".into(), Memory::default()),
            ("season".into(), Memory::default()),
        ]),
    )
    .unwrap();
    let batch = Batch {
        records: BTreeMap::from([
            ("tv:1".into(), json!({"id":1})),
            ("season:10".into(), json!({"id":10})),
        ]),
        ..Batch::default()
    };
    assert!(
        storage
            .commit("sync", &json!({"watermark":20}), &batch)
            .is_err()
    );
    assert_eq!(storage.index().unwrap().len(), 2);
    assert_eq!(storage.checkpoint("sync").unwrap()["watermark"], 10);
    storage
        .commit("sync", &json!({"watermark":20}), &batch)
        .unwrap();
    assert_eq!(storage.checkpoint("sync").unwrap()["watermark"], 20);
    assert_eq!(storage.index().unwrap().len(), 2);
}

#[test]
fn routing_rejects_unplanned_domains_before_any_destination_write() {
    use trakkin_mappings_ingestion::storage::RoutedStorage;
    let mut storage = RoutedStorage::new(
        Memory::default(),
        BTreeMap::from([("tv".into(), Memory::default())]),
    )
    .unwrap();
    let batch = Batch {
        records: BTreeMap::from([
            ("tv:1".into(), json!({"id":1})),
            ("unknown:2".into(), json!({"id":2})),
        ]),
        ..Batch::default()
    };
    assert!(
        storage
            .commit("sync", &json!({"watermark":100}), &batch)
            .is_err()
    );
    assert!(storage.index().unwrap().is_empty());
    assert_eq!(storage.checkpoint("sync").unwrap(), json!({}));
}

#[test]
fn completed_page_cursor_is_atomic_with_its_final_data_batch() {
    for batch_size in [1, 2] {
        let mut store = Memory {
            fail_after_commit: true,
            ..Memory::default()
        };
        store
            .checkpoints
            .insert("sync".into(), json!({"watermark":100}));
        let mut provider = Fixture::new(&["media:1", "media:2"]);
        provider.pages.push(vec![]);
        assert!(run(&mut provider, &mut store, Operation::Sync, batch_size, 200).is_err());
        let expected = if batch_size == 1 {
            Value::Null
        } else {
            json!({"page":1})
        };
        assert_eq!(store.checkpoints["sync"]["cursor"], expected);
        assert_eq!(store.commits.last().unwrap().0["cursor"], expected);
        assert_eq!(store.commits.last().unwrap().1, batch_size);
        run(&mut provider, &mut store, Operation::Sync, batch_size, 300).unwrap();
        assert_eq!(store.records.len(), 2);
        assert_eq!(store.checkpoints["sync"]["watermark"], 200);
    }
}

#[test]
fn malformed_checkpoints_fail_before_discovery_or_commit() {
    for checkpoint in [
        json!({"watermark":"100"}),
        json!({"pending":true,"watermark":100}),
        json!({"watermark":100,"unexpected":true}),
    ] {
        let mut store = Memory::default();
        store.checkpoints.insert("sync".into(), checkpoint.clone());
        let mut provider = Fixture::new(&["media:1"]);
        assert!(run(&mut provider, &mut store, Operation::Sync, 100, 200).is_err());
        assert!(provider.fetched.is_empty());
        assert!(store.entries.is_empty());
        assert_eq!(store.checkpoints["sync"], checkpoint);
    }
}

#[test]
fn progress_tracks_fetches_and_durable_changes() {
    let mut store = Memory::default();
    let mut provider = Fixture::new(&["media:1", "media:2"]);
    let mut events = Vec::new();
    let report = trakkin_mappings_ingestion::run(
        &mut provider,
        &mut store,
        Operation::Bootstrap,
        1,
        100,
        |phase, report| {
            events.push((
                phase.to_owned(),
                report.discovered,
                report.fetched,
                report.changed,
                report.complete,
            ));
        },
    )
    .unwrap();
    assert_eq!(events.first().unwrap().0, "loading checkpoint and index");
    assert!(
        events
            .iter()
            .any(|event| event.0 == "committing" && event.2 == 2 && event.3 == 0)
    );
    assert!(
        events
            .iter()
            .any(|event| event.0 == "committed" && event.3 == 1)
    );
    assert_eq!(events.last().unwrap(), &("complete".into(), 2, 2, 2, true));
    assert!(report.complete);
}

#[test]
fn complete_membership_removes_only_missing_children_of_that_parent() {
    let mut store = Memory::default();
    run(
        &mut Fixture::new(&["1", "2", "3"]),
        &mut store,
        Operation::Bootstrap,
        10,
        100,
    )
    .unwrap();
    store.entries.get_mut("1").unwrap().metadata = json!({"parent":"tv:1"});
    store.entries.get_mut("2").unwrap().metadata = json!({"parent":"tv:1"});
    store.entries.get_mut("3").unwrap().metadata = json!({"parent":"tv:2"});
    let mut provider = Fixture::new(&["1"]);
    provider.pages[0].push(Change::Membership {
        parent: "tv:1".into(),
        keys: ["1".into()].into(),
    });
    run(&mut provider, &mut store, Operation::Sync, 10, 200).unwrap();
    assert!(!store.entries["1"].deleted);
    assert!(store.entries["2"].deleted);
    assert!(!store.entries["3"].deleted);
    assert_eq!(store.tombstones["2"]["reason"], "parent_membership_absent");
}

#[test]
fn acquired_child_move_survives_old_parent_membership_removal() {
    for batch_size in [1, 10] {
        let mut store = Memory::default();
        let mut provider = Fixture::new(&["1"]);
        provider
            .descriptors
            .insert("1".into(), json!({"parent":"tv:1"}));
        run(
            &mut provider,
            &mut store,
            Operation::Bootstrap,
            batch_size,
            100,
        )
        .unwrap();
        provider
            .descriptors
            .insert("1".into(), json!({"parent":"tv:2"}));
        provider.pages[0].push(Change::Membership {
            parent: "tv:1".into(),
            keys: Default::default(),
        });
        run(&mut provider, &mut store, Operation::Sync, batch_size, 200).unwrap();
        assert!(!store.entries["1"].deleted);
        assert_eq!(store.entries["1"].metadata["parent"], "tv:2");
        assert!(store.tombstones.is_empty());
    }
}

#[test]
fn reserved_deletion_keys_fail_before_record_writes() {
    let mut store = Memory::default();
    let mut provider = Fixture::new(&[]);
    provider.pages[0].push(Change::Deleted {
        key: "@metadata/1".into(),
        metadata: json!({"reason":"deleted"}),
    });
    assert!(run(&mut provider, &mut store, Operation::Bootstrap, 10, 100).is_err());
    assert!(store.entries.is_empty());
    assert!(store.checkpoints["bootstrap"]["pending"].as_bool().unwrap());
}

#[test]
fn unchanged_acquisition_preserves_record_provenance_and_advances_run_checkpoint() {
    let mut store = Memory::default();
    let mut provider = Fixture::new(&["1"]);
    run(&mut provider, &mut store, Operation::Bootstrap, 10, 100).unwrap();
    let hash = store.entries["1"].hash.clone();
    let metadata_writes = store.metadata_writes;
    store.commits.clear();
    provider.pages = vec![
        vec![Change::Dirty("1".into())],
        vec![Change::Dirty("1".into())],
    ];
    let report = run(&mut provider, &mut store, Operation::Sync, 10, 200).unwrap();
    assert_eq!(report.changed, 0);
    assert_eq!(store.metadata_writes, metadata_writes);
    assert_eq!(store.commits.len(), 2);
    assert!(store.commits.iter().all(|(_, writes)| *writes == 0));
    assert_eq!(store.entries["1"].hash, hash);
    assert_eq!(store.entries["1"].metadata["materialized_at"], 100);
    assert_eq!(store.entries["1"].metadata["content_changed_at"], 100);
    assert_eq!(
        store.checkpoints["sync"]["last_run"]["report"]["complete"],
        true
    );
    assert_eq!(store.entries["1"].metadata["operation"], "bootstrap");
}

#[test]
fn changed_fetch_address_persists_without_rewriting_identical_content() {
    let mut store = Memory::default();
    let mut provider = Fixture::new(&["1"]);
    provider.descriptors.insert(
        "1".into(),
        json!({"address":"episode:1/1/1","parent":"tv:1"}),
    );
    run(&mut provider, &mut store, Operation::Bootstrap, 10, 100).unwrap();
    let writes = store.metadata_writes;
    provider.descriptors.insert(
        "1".into(),
        json!({"address":"episode:1/2/1","parent":"tv:1"}),
    );
    let report = run(&mut provider, &mut store, Operation::Sync, 10, 200).unwrap();
    assert_eq!(report.changed, 0);
    assert_eq!(store.metadata_writes, writes + 1);
    assert_eq!(store.entries["1"].metadata["address"], "episode:1/2/1");
    assert_eq!(store.entries["1"].metadata["materialized_at"], 200);
    assert_eq!(store.entries["1"].metadata["content_changed_at"], 100);
}

#[test]
fn not_found_tombstones_have_provenance_without_repeated_content_changes() {
    let mut store = Memory::default();
    let mut provider = Fixture::new(&["1"]);
    run(&mut provider, &mut store, Operation::Bootstrap, 10, 100).unwrap();
    provider.records.clear();
    run(&mut provider, &mut store, Operation::Sync, 10, 200).unwrap();
    assert!(store.entries["1"].deleted);
    assert_eq!(store.entries["1"].metadata["content_changed_at"], 200);
    run(&mut provider, &mut store, Operation::Sync, 10, 300).unwrap();
    assert_eq!(store.entries["1"].metadata["materialized_at"], 200);
    assert_eq!(store.entries["1"].metadata["content_changed_at"], 200);
}

#[test]
fn domain_reconciliation_confirms_only_selected_domain_absences() {
    use trakkin_mappings_ingestion::dataset::AcquisitionPlan;
    let mut store = Memory::default();
    let mut initial = AcquisitionPlan::new(
        Box::new(Fixture::new(&[
            "series:1", "series:2", "movie:1", "movie:3",
        ])),
        &["series"],
    )
    .unwrap();
    run(&mut initial, &mut store, Operation::Bootstrap, 100, 100).unwrap();
    assert_eq!(store.records.len(), 2);
    assert_eq!(store.records["series:1"]["id"], "series:1");
    let mut current = AcquisitionPlan::new(
        Box::new(Fixture::new(&["series:1", "movie:2", "movie:3"])),
        &["series"],
    )
    .unwrap();
    run(&mut current, &mut store, Operation::Reconcile, 100, 200).unwrap();
    assert_eq!(store.records.len(), 1);
    assert!(store.tombstones.contains_key("series:2"));
    assert!(!store.entries.contains_key("movie:3"));
    assert_eq!(store.checkpoints["sync"]["watermark"], 100);
}

#[test]
fn repeated_dirty_keys_fetch_once_and_preserve_event_order() {
    for deleted_last in [false, true] {
        let mut store = Memory::default();
        store
            .checkpoints
            .insert("sync".into(), json!({"watermark":100}));
        let mut provider = Fixture::new(&["media:1"]);
        let dirty = Change::Dirty("media:1".into());
        let deleted = Change::Deleted {
            key: "media:1".into(),
            metadata: json!({"reason":"merge"}),
        };
        provider.pages = vec![if deleted_last {
            vec![dirty.clone(), dirty, deleted]
        } else {
            vec![dirty.clone(), deleted, dirty]
        }];
        let report = run(&mut provider, &mut store, Operation::Sync, 100, 200).unwrap();
        assert_eq!(provider.fetched, vec!["media:1"]);
        assert_eq!(report.fetched, 1);
        assert_eq!(store.entries["media:1"].deleted, deleted_last);
    }
}

#[test]
fn canonical_hash_ignores_object_order_but_preserves_array_order() {
    assert_eq!(
        content_hash(&json!({"b":{"y":2,"x":1},"a":0})).unwrap(),
        content_hash(&json!({"a":0,"b":{"x":1,"y":2}})).unwrap()
    );
    assert_ne!(
        content_hash(&json!([1, 2])).unwrap(),
        content_hash(&json!([2, 1])).unwrap()
    );
}

#[test]
fn interrupted_bootstrap_reuses_committed_records_and_initial_watermark() {
    let mut store = Memory {
        fail_after_commit: true,
        ..Memory::default()
    };
    let mut provider = Fixture::new(&["media:1", "media:2"]);
    assert!(run(&mut provider, &mut store, Operation::Bootstrap, 1, 100).is_err());
    assert_eq!(store.records.len(), 1);
    provider.fetched.clear();
    let report = run(&mut provider, &mut store, Operation::Bootstrap, 1, 200).unwrap();
    assert_eq!(provider.fetched, vec!["media:2"]);
    assert!(report.complete);
    assert_eq!(store.checkpoints["sync"]["watermark"], 100);
    store.checkpoints.remove("sync");
    run(&mut provider, &mut store, Operation::Bootstrap, 1, 300).unwrap();
    assert_eq!(store.checkpoints["sync"]["watermark"], 100);
}

#[test]
fn partial_reconciliation_never_deletes_absent_records() {
    let mut store = Memory::default();
    run(
        &mut Fixture::new(&["media:1", "media:2"]),
        &mut store,
        Operation::Bootstrap,
        1,
        100,
    )
    .unwrap();
    let mut provider = Fixture::new(&["media:1"]);
    provider.pages.push(vec![]);
    provider.fail_page = Some(1);
    assert!(run(&mut provider, &mut store, Operation::Reconcile, 1, 200).is_err());
    assert!(store.records.contains_key("media:2"));
    assert!(store.tombstones.is_empty());
    assert_eq!(provider.fetched, vec!["media:1"]);
}

#[test]
fn reconciliation_repairs_existing_records_and_confirms_deletion() {
    let mut store = Memory::default();
    run(
        &mut Fixture::new(&["media:1", "media:2", "media:4"]),
        &mut store,
        Operation::Bootstrap,
        10,
        100,
    )
    .unwrap();
    let mut provider = Fixture::new(&["media:1", "media:3"]);
    provider
        .records
        .insert("media:1".into(), json!({"id":"media:1","title":"updated"}));
    provider
        .records
        .insert("media:4".into(), json!({"id":"media:4"}));
    let report = run(&mut provider, &mut store, Operation::Reconcile, 1, 200).unwrap();
    assert_eq!(report.fetched, 4);
    assert_eq!(
        provider.fetched,
        vec!["media:1", "media:3", "media:2", "media:4"]
    );
    assert_eq!(store.records["media:1"]["title"], "updated");
    assert!(store.entries["media:2"].deleted);
    assert!(!store.entries["media:4"].deleted);
    assert!(store.records.contains_key("media:3"));
    assert_eq!(store.checkpoints["sync"]["watermark"], 100);
}

#[test]
fn sync_deduplicates_and_commits_merge_metadata() {
    let mut store = Memory::default();
    run(
        &mut Fixture::new(&["media:1", "media:2"]),
        &mut store,
        Operation::Bootstrap,
        10,
        100,
    )
    .unwrap();
    let mut provider = Fixture::new(&["media:1"]);
    let metadata = json!({"mergeToId":3,"methodInt":3});
    provider.pages[0].push(Change::Deleted {
        key: "media:2".into(),
        metadata: metadata.clone(),
    });
    let report = run(&mut provider, &mut store, Operation::Sync, 10, 200).unwrap();
    assert_eq!(report.changed, 1);
    assert_eq!(store.tombstones["media:2"], metadata);
    assert_eq!(store.checkpoints["sync"]["watermark"], 200);
}

#[test]
fn failed_sync_keeps_old_watermark_and_replays_unacknowledged_page() {
    let mut store = Memory::default();
    run(
        &mut Fixture::new(&["media:1"]),
        &mut store,
        Operation::Bootstrap,
        10,
        100,
    )
    .unwrap();
    let mut provider = Fixture::new(&["media:2"]);
    provider.pages.push(vec![]);
    provider.fail_page = Some(1);
    assert!(run(&mut provider, &mut store, Operation::Sync, 1, 200).is_err());
    assert_eq!(store.checkpoints["sync"]["watermark"], 100);
    provider.fail_page = None;
    let report = run(&mut provider, &mut store, Operation::Sync, 1, 300).unwrap();
    assert_eq!(report.changed, 0);
    assert_eq!(store.checkpoints["sync"]["watermark"], 200);
}

#[test]
fn rejects_empty_catalogue_deletion_and_sync_without_bootstrap() {
    let mut store = Memory::default();
    assert!(run(&mut Fixture::new(&[]), &mut store, Operation::Sync, 1, 100).is_err());
    assert!(
        run(
            &mut Fixture::new(&[]),
            &mut store,
            Operation::Reconcile,
            1,
            100
        )
        .is_err()
    );
    assert!(
        run(
            &mut Fixture::new(&[]),
            &mut store,
            Operation::Bootstrap,
            0,
            100
        )
        .is_err()
    );
}
