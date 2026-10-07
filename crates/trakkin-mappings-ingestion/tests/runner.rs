mod common;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use trakkin_mappings_ingestion::{
    Change, Operation, Page, Provider, Report,
    storage::{Batch, Entry, Storage, Update},
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
    fn checkpoint(&mut self, operation: Operation) -> Result<Value> {
        Ok(self
            .checkpoints
            .get(operation.name())
            .cloned()
            .unwrap_or(json!({})))
    }
    fn commit(&mut self, operation: Operation, checkpoint: &Value, batch: &Batch) -> Result<()> {
        if self.fail_before_commit {
            self.fail_before_commit = false;
            bail!("Commit rejected before persistence");
        }
        batch.validate()?;
        batch.apply(&mut self.entries)?;
        let changed = batch.content_changes();
        self.metadata_writes += batch.updates.len();
        self.commits.push((checkpoint.clone(), changed));
        for (key, update) in &batch.updates {
            if let Some(content) = &update.content {
                let value = serde_json::from_str(content.payload())?;
                if content.deleted() {
                    self.records.remove(key);
                    self.tombstones.insert(key.clone(), value);
                } else {
                    self.records.insert(key.clone(), value);
                    self.tombstones.remove(key);
                }
            }
        }
        self.checkpoints
            .insert(operation.name().into(), checkpoint.clone());
        if self.fail_after_commit && changed > 0 {
            self.fail_after_commit = false;
            bail!("Lost acknowledgement after durable commit");
        }
        Ok(())
    }
}

struct Fixture {
    pages: Vec<Vec<Change>>,
    records: BTreeMap<String, Value>,
    fetched: Vec<String>,
    fetch_batches: Vec<usize>,
    fetch_batch_size: usize,
    fail_fetch_batch: Option<usize>,
    fail_page: Option<usize>,
    descriptors: BTreeMap<String, Value>,
    state: Value,
    discovered_state: Value,
    restored_states: Vec<Value>,
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
            fetch_batches: vec![],
            fetch_batch_size: 1000,
            fail_fetch_batch: None,
            fail_page: None,
            descriptors: BTreeMap::new(),
            state: Value::Null,
            discovered_state: Value::Null,
            restored_states: vec![],
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
    fn source(&self) -> &'static str {
        "com.thetvdb"
    }
    fn restore(
        &mut self,
        _operation: Operation,
        _index: &BTreeMap<String, Entry>,
        checkpoint: &Value,
    ) -> Result<()> {
        self.state = checkpoint.clone();
        self.restored_states.push(checkpoint.clone());
        Ok(())
    }
    fn checkpoint(&self) -> Value {
        self.state.clone()
    }
    fn metadata(&self, key: &str) -> Value {
        self.descriptors.get(key).cloned().unwrap_or(json!({}))
    }
    fn discover(
        &mut self,
        _operation: Operation,
        _watermark: Option<i64>,
        cursor: &Value,
        _until: i64,
    ) -> Result<Page> {
        self.state = self.discovered_state.clone();
        self.page(cursor)
    }
    fn fetch(&mut self, key: &str) -> Result<Option<Value>> {
        self.fetched.push(key.into());
        Ok(self.records.get(key).cloned())
    }
    fn fetch_batch_size(&self) -> usize {
        self.fetch_batch_size
    }
    fn fetch_many(&mut self, keys: &[String]) -> Result<BTreeMap<String, Option<Value>>> {
        self.fetch_batches.push(keys.len());
        if self.fail_fetch_batch == Some(self.fetch_batches.len()) {
            bail!("Interrupted acquisition");
        }
        keys.iter()
            .map(|key| Ok((key.clone(), self.fetch(key)?)))
            .collect()
    }
}

#[test]
fn provider_checkpoint_advances_only_after_success_and_replays_saved_state() {
    let mut store = Memory::default();
    let mut bootstrap = Fixture::new(&["media:1"]);
    bootstrap.discovered_state = json!({"after":1});
    run(&mut bootstrap, &mut store, Operation::Bootstrap, 100, 100).unwrap();
    assert_eq!(
        store.checkpoints["bootstrap"]["provider"],
        json!({"after":1})
    );
    assert_eq!(store.checkpoints["sync"]["provider"], json!({"after":1}));

    let mut sync = Fixture::new(&["media:2"]);
    sync.discovered_state = json!({"after":2});
    store.fail_after_commit = true;
    assert!(run(&mut sync, &mut store, Operation::Sync, 100, 200).is_err());
    assert_eq!(store.checkpoints["sync"]["provider"], json!({"after":1}));
    assert_eq!(store.checkpoints["sync"]["watermark"], 100);

    run(&mut sync, &mut store, Operation::Sync, 100, 300).unwrap();
    assert_eq!(
        sync.restored_states,
        vec![json!({"after":1}), json!({"after":1})]
    );
    assert_eq!(store.checkpoints["sync"]["provider"], json!({"after":2}));
    assert_eq!(store.checkpoints["sync"]["watermark"], 200);
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
    assert!(storage.checkpoint(Operation::Bootstrap).unwrap()["run"]["cursor"].is_null());
    assert!(storage.datasets_mut()["tv"].records.is_empty());
    assert_eq!(storage.datasets_mut()["season"].records.len(), 1);
    run(&mut provider, &mut storage, Operation::Bootstrap, 100, 200).unwrap();
    assert_eq!(storage.index().unwrap().len(), 3);
    assert_eq!(storage.datasets_mut()["tv"].records["1"]["id"], "tv:1");
    assert_eq!(
        storage.checkpoint(Operation::Sync).unwrap()["watermark"],
        100
    );
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
        updates: BTreeMap::from([
            (
                "tv:1".into(),
                Update::record(&json!({"id":1}), json!({})).unwrap(),
            ),
            (
                "season:10".into(),
                Update::record(&json!({"id":10}), json!({})).unwrap(),
            ),
        ]),
    };
    assert!(
        storage
            .commit(Operation::Sync, &json!({"watermark":20}), &batch)
            .is_err()
    );
    assert_eq!(storage.index().unwrap().len(), 2);
    assert_eq!(
        storage.checkpoint(Operation::Sync).unwrap()["watermark"],
        10
    );
    storage
        .commit(Operation::Sync, &json!({"watermark":20}), &batch)
        .unwrap();
    assert_eq!(
        storage.checkpoint(Operation::Sync).unwrap()["watermark"],
        20
    );
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
        updates: BTreeMap::from([
            (
                "tv:1".into(),
                Update::record(&json!({"id":1}), json!({})).unwrap(),
            ),
            (
                "unknown:2".into(),
                Update::record(&json!({"id":2}), json!({})).unwrap(),
            ),
        ]),
    };
    assert!(
        storage
            .commit(Operation::Sync, &json!({"watermark":100}), &batch)
            .is_err()
    );
    assert!(storage.index().unwrap().is_empty());
    assert_eq!(storage.checkpoint(Operation::Sync).unwrap(), json!({}));
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
        assert_eq!(store.checkpoints["sync"]["run"]["cursor"], expected);
        assert_eq!(store.commits.last().unwrap().0["run"]["cursor"], expected);
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
        json!({"run":{"started_at":100},"watermark":100}),
        json!({"run":{"id":"","started_at":100},"watermark":100}),
        json!({"run":{"id":"fixture","started_at":-1},"watermark":100}),
        json!({"watermark":100,"unexpected":true}),
    ] {
        let mut store = Memory::default();
        store.checkpoints.insert("sync".into(), checkpoint.clone());
        let mut provider = Fixture::new(&["media:1"]);
        assert!(run(&mut provider, &mut store, Operation::Sync, 100, 200).is_err());
        assert!(provider.fetched.is_empty());
        assert!(store.entries.is_empty());
        assert!(store.commits.is_empty());
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
                phase.to_string(),
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
            .any(|event| event.0 == "committed" && event.2 == 2 && event.3 == 1)
    );
    assert_eq!(events.last().unwrap(), &("complete".into(), 2, 2, 2, true));
    assert!(report.complete);
}

#[test]
fn acquisition_batches_are_bounded_independently_of_commit_batches() {
    let keys = ["media:1", "media:2", "media:3", "media:4", "media:5"];
    for fetch_batch_size in [1, 3, 1000, 2000, usize::MAX] {
        for batch_size in [1, 2, 4, 1000] {
            let mut store = Memory::default();
            let mut provider = Fixture::new(&keys);
            provider.fetch_batch_size = fetch_batch_size;
            let report = run(
                &mut provider,
                &mut store,
                Operation::Bootstrap,
                batch_size,
                100,
            )
            .unwrap();
            assert_eq!(
                provider.fetch_batches,
                keys.chunks(fetch_batch_size)
                    .map(<[_]>::len)
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                store
                    .commits
                    .iter()
                    .filter_map(|(_, count)| (*count > 0).then_some(*count))
                    .collect::<Vec<_>>(),
                keys.chunks(batch_size).map(<[_]>::len).collect::<Vec<_>>()
            );
            assert_eq!(report.fetched, 5);
            assert_eq!(report.changed, 5);
        }
    }
}

#[test]
fn acquisition_batches_exclude_skipped_and_deleted_keys() {
    let mut store = Memory::default();
    run(
        &mut Fixture::new(&["media:1"]),
        &mut store,
        Operation::Bootstrap,
        1,
        100,
    )
    .unwrap();
    let mut provider = Fixture::new(&[
        "media:1", "media:2", "media:3", "media:4", "media:5", "media:6",
    ]);
    provider.fetch_batch_size = 3;
    provider.pages[0][2] = Change::Deleted {
        key: "media:3".into(),
        metadata: json!({"reason":"deleted"}),
    };
    let report = run(&mut provider, &mut store, Operation::Bootstrap, 1, 200).unwrap();
    assert_eq!(provider.fetch_batches, vec![3, 1]);
    assert_eq!(
        provider.fetched,
        vec!["media:2", "media:4", "media:5", "media:6"]
    );
    assert_eq!(report.fetched, 4);
    assert!(store.entries["media:3"].deleted);
}

#[test]
fn failed_acquisition_keeps_page_cursor_behind_committed_chunks() {
    let mut store = Memory::default();
    let mut provider = Fixture::new(&["media:1", "media:2", "media:3", "media:4", "media:5"]);
    provider.fetch_batch_size = 2;
    provider.fail_fetch_batch = Some(2);
    provider.pages.push(vec![]);
    assert!(run(&mut provider, &mut store, Operation::Bootstrap, 1, 100).is_err());
    assert_eq!(store.records.len(), 2);
    assert!(store.checkpoints["bootstrap"]["run"]["cursor"].is_null());
    assert!(store.checkpoints["bootstrap"]["watermark"].is_null());
    provider.fail_fetch_batch = None;
    provider.fetch_batches.clear();
    provider.fetched.clear();
    run(&mut provider, &mut store, Operation::Bootstrap, 1, 200).unwrap();
    assert_eq!(provider.fetch_batches, vec![2, 1]);
    assert_eq!(provider.fetched, vec!["media:3", "media:4", "media:5"]);
    assert_eq!(store.records.len(), 5);
    assert_eq!(store.checkpoints["sync"]["watermark"], 100);
}

#[test]
fn anilist_request_packing_is_independent_of_commit_size() {
    use trakkin_mappings_ingestion::providers::AniList;

    let records: BTreeMap<_, _> = (1..=1100)
        .map(|media_id| (format!("media:{media_id}"), json!({"id":media_id})))
        .collect();
    let keys: Vec<_> = records.keys().cloned().collect();
    let mut catalogue = json!({"data":{"latest":{"id":1100}}});
    for offset in 0..160 {
        let media: Vec<_> = (offset * 50 + 1..=offset * 50 + 50)
            .filter(|media_id| *media_id <= 1100)
            .map(|media_id| json!({"id":media_id}))
            .collect();
        catalogue["data"][format!("batch{offset}")] = json!({"media":media});
    }
    let payload = |keys: &[String]| {
        let data: serde_json::Map<_, _> = keys
            .chunks(50)
            .enumerate()
            .map(|(offset, keys)| {
                let media: Vec<_> = keys.iter().map(|key| records[key].clone()).collect();
                (format!("batch{offset}"), json!({"media":media}))
            })
            .collect();
        json!({"data":data})
    };
    for batch_size in [1, 1000] {
        let (endpoint, handle) = common::server(vec![
            common::reply("latest:Media", catalogue.clone()),
            common::reply("batch10:Page", payload(&keys[..550])),
            common::reply("batch10:Page", payload(&keys[550..])),
        ]);
        let mut provider =
            AniList::with_endpoint_and_interval(&endpoint, std::time::Duration::ZERO).unwrap();
        assert_eq!(provider.fetch_batch_size(), 550);
        let mut store = Memory::default();
        let report = run(
            &mut provider,
            &mut store,
            Operation::Bootstrap,
            batch_size,
            100,
        )
        .unwrap();
        assert_eq!(report.fetched, 1100);
        assert_eq!(report.changed, 1100);
        assert_eq!(store.records.len(), 1100);
        assert!(store.commits.iter().all(|(_, count)| *count <= batch_size));
        handle.join().unwrap();
    }
}

#[test]
fn provider_acquisition_size_can_exceed_the_commit_limit() {
    let keys: Vec<_> = (1..=2500).map(|number| format!("media:{number}")).collect();
    let borrowed: Vec<_> = keys.iter().map(String::as_str).collect();
    let mut store = Memory::default();
    let mut provider = Fixture::new(&borrowed);
    provider.fetch_batch_size = 2000;
    let report = run(&mut provider, &mut store, Operation::Bootstrap, 1000, 100).unwrap();
    assert_eq!(provider.fetch_batches, vec![2000, 500]);
    assert_eq!(
        store
            .commits
            .iter()
            .filter_map(|(_, count)| (*count > 0).then_some(*count))
            .collect::<Vec<_>>(),
        vec![1000, 1000, 500]
    );
    assert_eq!(report.fetched, 2500);
    assert_eq!(report.changed, 2500);
}

#[test]
fn zero_acquisition_budget_fails_before_writes() {
    let mut store = Memory::default();
    let mut provider = Fixture::new(&["media:1"]);
    provider.fetch_batch_size = 0;
    assert!(run(&mut provider, &mut store, Operation::Bootstrap, 1, 100).is_err());
    assert!(provider.fetched.is_empty());
    assert!(store.commits.is_empty());
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
    assert!(store.checkpoints["bootstrap"]["run"].is_object());
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
    assert_eq!(store.commits.len(), 3);
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
fn unchanged_reconciliation_pages_do_not_commit_unused_cursors() {
    let mut store = Memory::default();
    let mut provider = Fixture::new(&["media:1"]);
    run(&mut provider, &mut store, Operation::Bootstrap, 10, 100).unwrap();
    let metadata_writes = store.metadata_writes;
    store.commits.clear();
    provider.pages = vec![
        vec![Change::Dirty("media:1".into())],
        vec![],
        vec![Change::Dirty("media:1".into())],
        vec![],
    ];
    let report = run(&mut provider, &mut store, Operation::Reconcile, 1, 200).unwrap();
    assert_eq!(report.changed, 0);
    assert_eq!(store.metadata_writes, metadata_writes);
    assert_eq!(store.commits.len(), 2);
    assert!(store.commits.iter().all(|(_, writes)| *writes == 0));
    assert!(store.commits[0].0["run"]["cursor"].is_null());
    assert_eq!(store.checkpoints["reconcile"]["watermark"], 200);
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
        assert_eq!(provider.fetched.len(), usize::from(!deleted_last));
        assert_eq!(report.fetched, u64::from(!deleted_last));
        assert_eq!(store.entries["media:1"].deleted, deleted_last);
    }
}

#[test]
fn final_event_wins_before_diffing_against_durable_content() {
    for batch_size in [1, 10] {
        for deleted_last in [false, true] {
            let mut store = Memory::default();
            let mut provider = Fixture::new(&["media:1"]);
            let dirty = Change::Dirty("media:1".into());
            let deleted = Change::Deleted {
                key: "media:1".into(),
                metadata: json!({"reason":"merge"}),
            };
            if deleted_last {
                provider.pages = vec![vec![deleted.clone()]];
            }
            run(&mut provider, &mut store, Operation::Bootstrap, 10, 100).unwrap();
            let metadata = store.entries["media:1"].metadata.clone();
            provider.pages = vec![if deleted_last {
                vec![dirty, deleted]
            } else {
                vec![deleted, dirty]
            }];
            let report = run(&mut provider, &mut store, Operation::Sync, batch_size, 200).unwrap();
            assert_eq!(report.changed, 0);
            assert_eq!(store.entries["media:1"].deleted, deleted_last);
            assert_eq!(store.entries["media:1"].metadata, metadata);
        }
    }
}

#[test]
fn interrupted_bootstrap_resumes_durable_pages_but_completed_bootstrap_scans_again() {
    let mut store = Memory {
        fail_after_commit: true,
        ..Memory::default()
    };
    let mut provider = Fixture::new(&["media:1", "media:2"]);
    provider.pages = vec![
        vec![Change::Dirty("media:1".into())],
        vec![Change::Dirty("media:2".into())],
    ];
    assert!(run(&mut provider, &mut store, Operation::Bootstrap, 100, 100).is_err());
    assert_eq!(
        store.checkpoints["bootstrap"]["run"]["cursor"],
        json!({"page":1})
    );
    provider.fail_page = Some(0);
    provider.fetched.clear();
    let report = run(&mut provider, &mut store, Operation::Bootstrap, 100, 200).unwrap();
    assert_eq!(report.discovered, 1);
    assert_eq!(provider.fetched, vec!["media:2"]);
    assert_eq!(store.checkpoints["sync"]["watermark"], 100);
    provider.fail_page = None;
    provider.pages[0].push(Change::Dirty("media:3".into()));
    provider
        .records
        .insert("media:3".into(), json!({"id":"media:3"}));
    provider.fetched.clear();
    run(&mut provider, &mut store, Operation::Bootstrap, 100, 300).unwrap();
    assert_eq!(provider.fetched, vec!["media:3"]);
    assert_eq!(store.checkpoints["sync"]["watermark"], 100);
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
    assert_eq!(store.checkpoints["sync"]["watermark"], 300);
}

#[test]
fn empty_and_skipped_pages_save_their_resume_cursor() {
    for skipped in [false, true] {
        let mut store = Memory::default();
        let mut provider = Fixture::new(&["media:1"]);
        run(&mut provider, &mut store, Operation::Bootstrap, 10, 100).unwrap();
        provider.pages = vec![
            if skipped {
                vec![Change::Dirty("media:1".into())]
            } else {
                vec![]
            },
            vec![],
        ];
        provider.fail_page = Some(1);
        assert!(run(&mut provider, &mut store, Operation::Bootstrap, 10, 200).is_err());
        assert_eq!(
            store.checkpoints["bootstrap"]["run"]["cursor"],
            json!({"page":1})
        );
        assert_eq!(store.checkpoints["bootstrap"]["run"]["started_at"], 200);
        provider.fail_page = Some(0);
        run(&mut provider, &mut store, Operation::Bootstrap, 10, 300).unwrap();
        assert_eq!(store.checkpoints["bootstrap"]["watermark"], 200);
    }
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
fn reconciliation_does_not_infer_deletions_from_provider_catalogue() {
    let mut store = Memory::default();
    run(
        &mut Fixture::new(&["media:1", "media:2"]),
        &mut store,
        Operation::Bootstrap,
        10,
        100,
    )
    .unwrap();
    let report = run(
        &mut Fixture::new(&["media:1"]),
        &mut store,
        Operation::Reconcile,
        10,
        200,
    )
    .unwrap();
    assert!(report.complete);
    assert_eq!(report.fetched, 1);
    assert!(store.records.contains_key("media:2"));
    assert!(store.tombstones.is_empty());
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
    provider.pages.push(vec![
        Change::Dirty("media:2".into()),
        Change::Dirty("media:4".into()),
    ]);
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
fn runner_counts_prepared_content_changes_after_commit() {
    let mut store = Memory::default();
    let mut provider = Fixture::new(&["media:1", "media:2"]);
    let report = run(&mut provider, &mut store, Operation::Bootstrap, 1, 100).unwrap();
    assert_eq!(report.changed, 2);
    assert_eq!(store.commits.len(), 5);
    provider
        .records
        .insert("media:1".into(), json!({"id":"updated"}));
    let report = run(&mut provider, &mut store, Operation::Sync, 1, 200).unwrap();
    assert_eq!(report.changed, 1);
    assert_eq!(store.commits.len(), 8);
    assert_eq!(store.records["media:1"]["id"], "updated");
}

#[test]
fn rejects_sync_without_bootstrap_and_invalid_batch_size() {
    let mut store = Memory::default();
    assert!(run(&mut Fixture::new(&[]), &mut store, Operation::Sync, 1, 100).is_err());
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
