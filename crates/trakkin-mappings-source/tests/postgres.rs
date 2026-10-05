use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use trakkin_mappings_source::{
    Change, FetchRequest, Membership, Mode, Page, Report, Resume, Source, SyncOptions,
    storage::{Batch, Commit, Cursor, Postgres, Update},
};
use trakkin_mappings_test_support::Database;

fn database(name: &str) -> (Database, String, Postgres) {
    let database = Database::start();
    let source = format!("test.{name}");
    let store = Postgres::connect(database.url.as_str(), &source, &["media", "episode"]).unwrap();
    (database, source, store)
}

fn reopen(database: &Database, source: &str, domains: &[&str]) -> Postgres {
    Postgres::connect(database.url.as_str(), source, domains).unwrap()
}

fn commit(store: &mut Postgres, updates: BTreeMap<String, Update>) -> Result<()> {
    store.commit(Commit {
        state: &json!({"watermark":100}),
        batch: &Batch { updates },
        seen: &[],
        run_id: "fixture",
    })
}

struct Fixture {
    source: String,
    pages: Vec<Vec<String>>,
    records: BTreeMap<String, Value>,
    metadata: BTreeMap<String, Value>,
    fail_key: Option<String>,
    fail_page: Option<usize>,
    fetched: Vec<String>,
    requests: Vec<FetchRequest>,
    incomplete: bool,
    membership: Option<(String, BTreeSet<String>)>,
    membership_pages: Vec<Option<(String, BTreeSet<String>)>>,
    required_domains: Vec<String>,
    modes: Vec<Mode>,
    watermarks: Vec<Option<i64>>,
    provider_checkpoint: Value,
    restored: Value,
}

impl Fixture {
    fn new(source: &str, keys: &[&str]) -> Self {
        Self {
            source: source.to_owned(),
            pages: vec![keys.iter().map(|key| (*key).into()).collect()],
            records: keys
                .iter()
                .map(|key| ((*key).into(), json!({"id":key})))
                .collect(),
            metadata: BTreeMap::new(),
            fail_key: None,
            fail_page: None,
            fetched: vec![],
            requests: vec![],
            incomplete: false,
            membership: None,
            membership_pages: vec![],
            required_domains: vec![],
            modes: vec![],
            watermarks: vec![],
            provider_checkpoint: Value::Null,
            restored: Value::Null,
        }
    }
    fn sync(&mut self, store: &mut Postgres, options: SyncOptions, now: i64) -> Result<Report> {
        trakkin_mappings_source::sync(self, store, options, now, |_, _| {}).map(|run| run.report)
    }
}

impl Source for Fixture {
    fn source(&self) -> &str {
        &self.source
    }
    fn descriptor(&self, key: &str) -> Option<Value> {
        self.metadata.get(key).cloned()
    }
    fn resume(&self, _: Mode) -> Resume {
        Resume::Cursor
    }
    fn restore(&mut self, checkpoint: &Value) -> Result<()> {
        self.restored = checkpoint.clone();
        Ok(())
    }
    fn checkpoint(&self) -> Value {
        self.provider_checkpoint.clone()
    }
    fn discover(
        &mut self,
        mode: Mode,
        watermark: Option<i64>,
        cursor: &Value,
        _: i64,
    ) -> Result<Page> {
        self.modes.push(mode);
        self.watermarks.push(watermark);
        let page = cursor.as_u64().unwrap_or(0) as usize;
        if self.fail_page == Some(page) {
            bail!("Interrupted enumeration");
        }
        if !self.membership_pages.is_empty() {
            self.membership = self.membership_pages[page].clone();
        }
        Ok(Page {
            changes: self.pages[page]
                .iter()
                .cloned()
                .map(Change::Dirty)
                .collect(),
            next: (page + 1 < self.pages.len()).then(|| json!(page + 1)),
        })
    }
    fn fetch(&mut self, requests: &[FetchRequest]) -> Result<Vec<Option<Value>>> {
        self.requests.extend_from_slice(requests);
        let mut records = requests
            .iter()
            .map(|request| {
                if self.fail_key.as_deref() == Some(request.key.as_str()) {
                    bail!("Interrupted fetch");
                }
                self.fetched.push(request.key.clone());
                Ok(self.records.get(&request.key).cloned())
            })
            .collect::<Result<Vec<_>>>()?;
        if self.incomplete {
            records.pop();
        }
        Ok(records)
    }
    fn memberships(&self) -> Vec<Membership> {
        self.membership
            .as_ref()
            .map(|(parent, members)| {
                vec![Membership {
                    parent: parent.clone(),
                    members: members.clone(),
                }]
            })
            .unwrap_or_default()
    }
    fn required_catalogue_domains(&self) -> Vec<&str> {
        self.required_domains.iter().map(String::as_str).collect()
    }
}

#[test]
fn batches_and_sync_state_roll_back_together() {
    let (_database, _, mut store) = database("atomic");
    store.begin().unwrap();
    let broken = BTreeMap::from([
        (
            "media:1".into(),
            Update::record(&json!({"id":1}), json!({})).unwrap(),
        ),
        (
            "media:missing".into(),
            Update {
                content: None,
                metadata: json!({}),
            },
        ),
    ]);
    assert!(commit(&mut store, broken).is_err());
    assert_eq!(store.revision().unwrap(), 0);
    assert!(store.inspect("", 10, true).unwrap().is_empty());
    assert_eq!(store.state().unwrap(), json!({}));
    commit(
        &mut store,
        BTreeMap::from([
            (
                "media:1".into(),
                Update::record(&json!({"id":1}), json!({})).unwrap(),
            ),
            (
                "episode:2".into(),
                Update::record(&json!({"id":2}), json!({})).unwrap(),
            ),
        ]),
    )
    .unwrap();
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(store.state().unwrap()["watermark"], 100);
    assert_eq!(store.validate().unwrap(), 2);
}

#[test]
fn source_locks_exclude_other_domains_but_allow_readers_and_release_on_disconnect() {
    let (database, source, mut first) = database("locks");
    first.begin().unwrap();
    let mut other = reopen(&database, &source, &["media"]);
    assert!(other.begin().is_err());
    assert!(other.inspect("", 1, true).unwrap().is_empty());
    drop(first);
    other.begin().unwrap();
}

#[test]
fn interrupted_initial_sync_replays_without_refetching_records_committed_by_that_run() {
    let (database, source, mut store) = database("replay");
    let mut fixture = Fixture::new(&source, &["media:1", "media:2", "media:3", "media:4"]);
    fixture.fail_key = Some("media:3".into());
    assert!(
        fixture
            .sync(
                &mut store,
                SyncOptions {
                    full: false,
                    batch_size: 2,
                    ..SyncOptions::default()
                },
                100
            )
            .is_err()
    );
    assert_eq!(store.inspect("", 10, true).unwrap().len(), 2);
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(
        store.state().unwrap()["run"]["progress"],
        json!({"phase":"discover","cursor":null})
    );
    drop(store);
    let mut store = reopen(&database, &source, &["media", "episode"]);
    fixture.fail_key = None;
    fixture.fetched.clear();
    let report = fixture
        .sync(
            &mut store,
            SyncOptions {
                batch_size: 2,
                ..SyncOptions::default()
            },
            200,
        )
        .unwrap();
    assert!(report.complete);
    assert_eq!(report.discovered, 4);
    assert_eq!(report.fetched, 4);
    assert_eq!(report.changed, 4);
    assert_eq!(fixture.fetched, ["media:3", "media:4"]);
    assert_eq!(store.revision().unwrap(), 2);
    assert_eq!(store.state().unwrap()["watermark"], 100);
}

#[test]
fn unchanged_sync_advances_watermark_without_creating_payload_revisions() {
    let (_database, source, mut store) = database("noop");
    let mut fixture = Fixture::new(&source, &["media:1"]);
    fixture
        .sync(
            &mut store,
            SyncOptions {
                full: false,
                batch_size: 100,
                ..SyncOptions::default()
            },
            100,
        )
        .unwrap();
    let report = fixture
        .sync(
            &mut store,
            SyncOptions {
                full: false,
                batch_size: 100,
                ..SyncOptions::default()
            },
            200,
        )
        .unwrap();
    assert_eq!(report.changed, 0);
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(store.state().unwrap()["watermark"], 200);
    let records = store
        .changes(&Cursor::default(), None, 100)
        .unwrap()
        .records;
    assert_eq!(records.len(), 1);
}

#[test]
fn acquisition_uses_fresh_descriptors_and_falls_back_to_stored_addresses() {
    let (_database, source, mut store) = database("descriptors");
    let mut fixture = Fixture::new(&source, &["media:1", "media:2"]);
    fixture
        .metadata
        .insert("media:1".into(), json!({"address":"old"}));
    fixture
        .metadata
        .insert("media:2".into(), json!({"address":"stored"}));
    fixture
        .sync(
            &mut store,
            SyncOptions {
                full: false,
                batch_size: 100,
                ..SyncOptions::default()
            },
            100,
        )
        .unwrap();
    fixture.metadata = BTreeMap::from([("media:1".into(), json!({"address":"fresh"}))]);
    fixture.requests.clear();
    let report = fixture
        .sync(
            &mut store,
            SyncOptions {
                full: false,
                batch_size: 100,
                ..SyncOptions::default()
            },
            200,
        )
        .unwrap();
    assert_eq!(report.changed, 0);
    assert_eq!(store.revision().unwrap(), 2);
    assert_eq!(fixture.requests[0].key, "media:1");
    assert_eq!(fixture.requests[0].descriptor["address"], "fresh");
    assert_eq!(fixture.requests[1].key, "media:2");
    assert_eq!(fixture.requests[1].descriptor["address"], "stored");
    fixture.metadata.clear();
    fixture.requests.clear();
    fixture
        .sync(
            &mut store,
            SyncOptions {
                full: false,
                batch_size: 100,
                ..SyncOptions::default()
            },
            300,
        )
        .unwrap();
    assert_eq!(store.revision().unwrap(), 2);
    assert_eq!(fixture.requests[0].descriptor["address"], "fresh");
}

#[test]
fn incomplete_acquisition_batches_commit_no_records_and_can_retry() {
    let (_database, source, mut store) = database("incomplete_batch");
    let mut fixture = Fixture::new(&source, &["media:1", "media:2"]);
    fixture.incomplete = true;
    let error = fixture
        .sync(
            &mut store,
            SyncOptions {
                full: false,
                batch_size: 100,
                ..SyncOptions::default()
            },
            100,
        )
        .unwrap_err();
    assert!(error.to_string().contains("incomplete batch"));
    assert_eq!(store.revision().unwrap(), 0);
    assert!(store.inspect("", 10, true).unwrap().is_empty());
    assert!(store.state().unwrap()["watermark"].is_null());
    fixture.incomplete = false;
    fixture
        .sync(
            &mut store,
            SyncOptions {
                full: false,
                batch_size: 100,
                ..SyncOptions::default()
            },
            200,
        )
        .unwrap();
    assert_eq!(store.validate().unwrap(), 2);
    assert_eq!(store.state().unwrap()["watermark"], 100);
}

#[test]
fn reconciliation_never_sweeps_an_incomplete_catalogue_and_can_resume() {
    let (_database, source, mut store) = database("reconcile");
    Fixture::new(&source, &["media:1", "media:2", "media:gone"])
        .sync(
            &mut store,
            SyncOptions {
                full: false,
                batch_size: 100,
                ..SyncOptions::default()
            },
            100,
        )
        .unwrap();
    let mut fixture = Fixture::new(&source, &["media:1", "media:2"]);
    fixture.pages = vec![vec!["media:1".into()], vec!["media:2".into()]];
    fixture.fail_page = Some(1);
    assert!(
        fixture
            .sync(
                &mut store,
                SyncOptions {
                    full: true,
                    batch_size: 100,
                    ..SyncOptions::default()
                },
                200
            )
            .is_err()
    );
    assert_eq!(store.inspect("", 10, false).unwrap().len(), 3);
    assert_eq!(store.state().unwrap()["run"]["mode"], "full");
    fixture.fail_page = None;
    fixture
        .sync(&mut store, SyncOptions::default(), 300)
        .unwrap();
    let records = store.inspect("", 10, true).unwrap();
    assert_eq!(records.len(), 3);
    assert!(
        records
            .iter()
            .find(|record| record.key == "media:gone")
            .unwrap()
            .deleted
    );
    assert_eq!(store.inspect("", 10, false).unwrap().len(), 2);
}

#[test]
fn empty_catalogues_cannot_tombstone_existing_records() {
    let (_database, source, mut store) = database("empty");
    Fixture::new(&source, &["media:1"])
        .sync(
            &mut store,
            SyncOptions {
                full: false,
                batch_size: 100,
                ..SyncOptions::default()
            },
            100,
        )
        .unwrap();
    assert!(
        Fixture::new(&source, &[])
            .sync(
                &mut store,
                SyncOptions {
                    full: true,
                    batch_size: 100,
                    ..SyncOptions::default()
                },
                200
            )
            .is_err()
    );
    assert!(!store.inspect("", 10, true).unwrap()[0].deleted);
    assert_eq!(store.revision().unwrap(), 1);
}

#[test]
fn restart_preserves_committed_data_and_watermarks_and_cannot_interrupt_a_writer() {
    let (database, source, mut store) = database("restart");
    Fixture::new(&source, &["media:1"])
        .sync(&mut store, SyncOptions::default(), 100)
        .unwrap();
    let mut fixture = Fixture::new(&source, &["media:1", "media:2"]);
    fixture.fail_key = Some("media:2".into());
    let options = SyncOptions {
        batch_size: 1,
        ..SyncOptions::default()
    };
    assert!(fixture.sync(&mut store, options, 200).is_err());
    let old_id = store.state().unwrap()["run"]["id"].clone();
    let mut other = reopen(&database, &source, &["media", "episode"]);
    let restart = SyncOptions {
        restart: true,
        ..options
    };
    assert!(fixture.sync(&mut other, restart, 300).is_err());
    drop(store);
    let revision = other.revision().unwrap();
    assert!(fixture.sync(&mut other, restart, 300).is_err());
    let checkpoint = other.state().unwrap();
    assert_ne!(checkpoint["run"]["id"], old_id);
    assert_eq!(checkpoint["run"]["started_at"], 300);
    assert_eq!(checkpoint["watermark"], 100);
    assert_eq!(other.revision().unwrap(), revision);
    assert_eq!(other.inspect("", 10, false).unwrap().len(), 1);
    fixture.fail_key = None;
    fixture.sync(&mut other, options, 400).unwrap();
    assert_eq!(other.state().unwrap()["watermark"], 300);
    assert_eq!(other.validate().unwrap(), 2);
}

#[test]
fn a_full_sync_advances_the_shared_baseline_and_refreshes_existing_records() {
    let (_database, source, mut store) = database("full_baseline");
    let mut fixture = Fixture::new(&source, &["media:1"]);
    fixture
        .sync(&mut store, SyncOptions::default(), 100)
        .unwrap();
    fixture
        .records
        .insert("media:1".into(), json!({"id":"media:1","updated":true}));
    fixture.provider_checkpoint = json!({"after":50});
    fixture
        .sync(
            &mut store,
            SyncOptions {
                full: true,
                ..SyncOptions::default()
            },
            200,
        )
        .unwrap();
    assert_eq!(store.state().unwrap()["watermark"], 200);
    assert_eq!(
        store.get("media:1").unwrap().unwrap().payload["updated"],
        true
    );
    fixture
        .sync(&mut store, SyncOptions::default(), 300)
        .unwrap();
    assert_eq!(fixture.modes, [Mode::Full, Mode::Full, Mode::Incremental]);
    assert_eq!(fixture.watermarks, [None, Some(100), Some(200)]);
    assert_eq!(fixture.restored, json!({"after":50}));
}

#[test]
fn full_sync_resumes_its_absence_sweep_and_preserves_completed_discovery_state() {
    let (database, source, mut store) = database("absence_resume");
    Fixture::new(&source, &["media:1", "media:gone"])
        .sync(&mut store, SyncOptions::default(), 100)
        .unwrap();
    let mut fixture = Fixture::new(&source, &["media:1"]);
    fixture.provider_checkpoint = json!({"after":500});
    fixture.fail_key = Some("media:gone".into());
    assert!(
        fixture
            .sync(
                &mut store,
                SyncOptions {
                    full: true,
                    ..SyncOptions::default()
                },
                200
            )
            .is_err()
    );
    assert_eq!(
        store.state().unwrap()["run"]["progress"]["phase"],
        "absence"
    );
    drop(store);
    let mut store = reopen(&database, &source, &["media", "episode"]);
    let mut fixture = Fixture::new(&source, &["media:1"]);
    fixture
        .sync(&mut store, SyncOptions::default(), 300)
        .unwrap();
    assert!(fixture.modes.is_empty());
    assert_eq!(fixture.restored, json!({"after":500}));
    assert_eq!(store.state().unwrap()["provider"], json!({"after":500}));
    assert_eq!(store.state().unwrap()["watermark"], 200);
    assert!(store.get("media:gone").unwrap().unwrap().deleted);
}

#[test]
fn changing_a_pending_incremental_sync_to_full_requires_explicit_restart() {
    let (_database, source, mut store) = database("switch_mode");
    Fixture::new(&source, &["media:1"])
        .sync(&mut store, SyncOptions::default(), 100)
        .unwrap();
    let mut fixture = Fixture::new(&source, &["media:1", "media:2"]);
    fixture.fail_key = Some("media:2".into());
    assert!(
        fixture
            .sync(&mut store, SyncOptions::default(), 200)
            .is_err()
    );
    let state = store.state().unwrap();
    let full = SyncOptions {
        full: true,
        ..SyncOptions::default()
    };
    let error = fixture.sync(&mut store, full, 300).unwrap_err();
    assert!(error.to_string().contains("--full --restart"));
    assert_eq!(store.state().unwrap(), state);
    fixture.fail_key = None;
    fixture
        .sync(
            &mut store,
            SyncOptions {
                restart: true,
                ..full
            },
            400,
        )
        .unwrap();
    assert_eq!(store.state().unwrap()["watermark"], 400);
    assert_eq!(store.state().unwrap()["last_run"]["mode"], "full");
}

#[test]
fn restart_before_initialization_keeps_records_but_starts_a_new_full_scan() {
    let (_database, source, mut store) = database("restart_initial");
    let mut fixture = Fixture::new(&source, &["media:1", "media:2", "media:3"]);
    fixture.fail_key = Some("media:3".into());
    let options = SyncOptions {
        batch_size: 2,
        ..SyncOptions::default()
    };
    assert!(fixture.sync(&mut store, options, 100).is_err());
    let old_id = store.state().unwrap()["run"]["id"].clone();
    fixture.fail_page = Some(0);
    assert!(
        fixture
            .sync(
                &mut store,
                SyncOptions {
                    restart: true,
                    ..options
                },
                200
            )
            .is_err()
    );
    let state = store.state().unwrap();
    assert_ne!(state["run"]["id"], old_id);
    assert_eq!(state["run"]["mode"], "full");
    assert!(state["watermark"].is_null());
    assert_eq!(store.inspect("", 10, false).unwrap().len(), 2);
    fixture.fail_page = None;
    fixture.fail_key = None;
    fixture.fetched.clear();
    fixture.sync(&mut store, options, 300).unwrap();
    assert_eq!(fixture.fetched, ["media:1", "media:2", "media:3"]);
    assert_eq!(store.state().unwrap()["watermark"], 200);
}

#[test]
fn unfinished_sync_blocks_other_domain_selections_until_the_original_selection_finishes() {
    let (database, source, mut store) = database("pending_scope");
    let mut fixture = Fixture::new(&source, &["media:1"]);
    fixture.fail_page = Some(0);
    assert!(
        fixture
            .sync(&mut store, SyncOptions::default(), 100)
            .is_err()
    );
    drop(store);
    let mut other = reopen(&database, &source, &["episode"]);
    let error = fixture
        .sync(
            &mut other,
            SyncOptions {
                restart: true,
                ..SyncOptions::default()
            },
            200,
        )
        .unwrap_err();
    assert!(error.to_string().contains("another domain selection"));
    drop(other);
    let mut store = reopen(&database, &source, &["media", "episode"]);
    fixture.fail_page = None;
    fixture
        .sync(&mut store, SyncOptions::default(), 200)
        .unwrap();
    drop(store);
    let mut other = reopen(&database, &source, &["episode"]);
    other.begin().unwrap();
}

#[test]
fn parent_memberships_are_reconciled_with_bounded_database_reads() {
    let (_database, source, mut store) = database("parent");
    store.begin().unwrap();
    for keys in (0..1205).collect::<Vec<_>>().chunks(1000) {
        commit(
            &mut store,
            keys.iter()
                .map(|number| {
                    (
                        format!("episode:{number:04}"),
                        Update::record(&json!({"id":number}), json!({"parent":"media:1"})).unwrap(),
                    )
                })
                .collect(),
        )
        .unwrap();
    }
    let mut fixture = Fixture::new(&source, &["media:1"]);
    fixture.membership = Some(("media:1".into(), BTreeSet::new()));
    fixture
        .sync(
            &mut store,
            SyncOptions {
                full: false,
                batch_size: 100,
                ..SyncOptions::default()
            },
            200,
        )
        .unwrap();
    assert!(store.children("media:1", "").unwrap().is_empty());
    assert_eq!(store.inspect("", 1000, false).unwrap().len(), 1);
    assert_eq!(store.validate().unwrap(), 1206);
}

#[test]
fn status_reports_successful_and_pending_domain_selections_without_loading_payloads() {
    let (database, source, mut store) = database("status");
    let empty = trakkin_mappings_source::status(&mut store).unwrap();
    assert_eq!(empty.source, source);
    assert_eq!(empty.revision, 0);
    assert!(empty.scopes.is_empty());
    drop(store);
    let mut media = reopen(&database, &source, &["media"]);
    Fixture::new(&source, &["media:1"])
        .sync(&mut media, SyncOptions::default(), 100)
        .unwrap();
    drop(media);
    let mut episodes = reopen(&database, &source, &["episode"]);
    let mut fixture = Fixture::new(&source, &["episode:1", "episode:2"]);
    fixture.fail_key = Some("episode:2".into());
    assert!(
        fixture
            .sync(
                &mut episodes,
                SyncOptions {
                    batch_size: 1,
                    ..SyncOptions::default()
                },
                200
            )
            .is_err()
    );
    let status = trakkin_mappings_source::status(&mut episodes).unwrap();
    assert_eq!(status.scopes.len(), 2);
    let pending = &status.scopes[0];
    assert_eq!(pending.domains, ["episode"]);
    assert!(!pending.initialized);
    let active = pending.active.as_ref().unwrap();
    assert_eq!(active.mode, Mode::Full);
    assert_eq!(active.started_at, 200);
    assert_eq!(active.report.fetched, 1);
    assert_eq!(active.report.changed, 1);
    assert!(!active.report.complete);
    let completed = &status.scopes[1];
    assert_eq!(completed.domains, ["media"]);
    assert!(completed.initialized);
    assert_eq!(completed.watermark, Some(100));
    assert!(completed.active.is_none());
    assert_eq!(completed.last_run.as_ref().unwrap().mode, Mode::Full);
    assert!(episodes.get("media:1").unwrap().is_none());
    assert!(episodes.get("episode:1").unwrap().is_some());
}

#[test]
fn change_paging_pins_a_revision_and_preserves_metadata_and_deletions() {
    let (database, source, mut store) = database("changes");
    store.begin().unwrap();
    commit(
        &mut store,
        BTreeMap::from([
            (
                "media:1".into(),
                Update::record(&json!({"id":1}), json!({"parent":"media:old"})).unwrap(),
            ),
            (
                "media:2".into(),
                Update::record(&json!({"id":2}), json!({})).unwrap(),
            ),
        ]),
    )
    .unwrap();
    let first = store.changes(&Cursor::default(), None, 1).unwrap();
    commit(
        &mut store,
        BTreeMap::from([
            (
                "media:1".into(),
                Update {
                    content: None,
                    metadata: json!({"parent":"media:new"}),
                },
            ),
            (
                "media:2".into(),
                Update::deletion(&json!({"reason":"gone"}), json!({})).unwrap(),
            ),
        ]),
    )
    .unwrap();
    let second = store
        .changes(&first.cursor, Some(first.through), 1)
        .unwrap();
    assert_eq!(first.through, 1);
    assert_eq!(second.records[0].key, "media:2");
    assert_eq!(second.records[0].revision, 1);
    let next = store.changes(&second.cursor, None, 100).unwrap();
    assert_eq!(next.records.len(), 2);
    assert_eq!(next.records[0].payload, json!({"id":1}));
    assert_eq!(next.records[0].metadata["parent"], "media:new");
    assert!(next.records[1].deleted);
    let mut client = postgres::Client::connect(database.url.as_str(), postgres::NoTls).unwrap();
    client
        .execute(
            "UPDATE ingestion.sources SET history_floor=1 WHERE source=$1",
            &[&source],
        )
        .unwrap();
    assert!(store.changes(&Cursor::default(), None, 100).is_err());
    assert!(store.changes(&first.cursor, None, 100).is_err());
    assert!(
        store
            .changes(
                &Cursor {
                    revision: 1,
                    key: None
                },
                None,
                100
            )
            .is_ok()
    );
    assert_eq!(store.validate().unwrap(), 2);
}

#[test]
fn change_cursors_advance_across_empty_scopes_without_replaying_completed_revisions() {
    let (database, source, mut store) = database("empty_changes");
    store.begin().unwrap();
    commit(
        &mut store,
        BTreeMap::from([(
            "episode:1".into(),
            Update::record(&json!({"id":1}), json!({})).unwrap(),
        )]),
    )
    .unwrap();
    let mut media = reopen(&database, &source, &["media"]);
    let empty = media.changes(&Cursor::default(), None, 10).unwrap();
    assert!(empty.complete && empty.records.is_empty());
    assert_eq!(empty.cursor.revision, 1);
    assert!(empty.cursor.key.is_none());
    commit(
        &mut store,
        BTreeMap::from([(
            "media:1".into(),
            Update::record(&json!({"id":1}), json!({})).unwrap(),
        )]),
    )
    .unwrap();
    let changed = media.changes(&empty.cursor, None, 10).unwrap();
    assert_eq!(changed.records.len(), 1);
    assert!(changed.complete);
    let idle = media.changes(&changed.cursor, None, 10).unwrap();
    assert!(idle.records.is_empty() && idle.complete);
}

#[test]
fn a_nonempty_domain_cannot_hide_an_empty_required_catalogue() {
    let (_database, source, mut store) = database("empty_domain");
    Fixture::new(&source, &["media:1", "episode:1"])
        .sync(
            &mut store,
            SyncOptions {
                full: false,
                batch_size: 100,
                ..SyncOptions::default()
            },
            100,
        )
        .unwrap();
    let mut fixture = Fixture::new(&source, &["media:1"]);
    fixture.required_domains = vec!["media".into(), "episode".into()];
    assert!(
        fixture
            .sync(
                &mut store,
                SyncOptions {
                    full: true,
                    batch_size: 100,
                    ..SyncOptions::default()
                },
                200
            )
            .is_err()
    );
    assert_eq!(store.inspect("", 10, false).unwrap().len(), 2);
}

#[test]
fn child_moves_survive_parent_updates_in_either_page_order() {
    for old_first in [true, false] {
        let (_database, source, mut store) = database("move");
        store.begin().unwrap();
        commit(
            &mut store,
            BTreeMap::from([
                (
                    "episode:100".into(),
                    Update::record(&json!({"id":100}), json!({"parent":"media:old"})).unwrap(),
                ),
                (
                    "episode:200".into(),
                    Update::record(&json!({"id":200}), json!({"parent":"media:unrelated"}))
                        .unwrap(),
                ),
            ]),
        )
        .unwrap();
        let mut fixture = Fixture::new(&source, &["media:old", "media:new", "episode:100"]);
        fixture.pages = vec![
            vec!["media:old".into()],
            vec!["media:new".into(), "episode:100".into()],
        ];
        fixture.membership_pages = vec![
            Some(("media:old".into(), BTreeSet::new())),
            Some(("media:new".into(), BTreeSet::from(["episode:100".into()]))),
        ];
        fixture
            .metadata
            .insert("episode:100".into(), json!({"parent":"media:new"}));
        if !old_first {
            fixture.pages.reverse();
            fixture.membership_pages.reverse();
        }
        fixture
            .sync(
                &mut store,
                SyncOptions {
                    full: false,
                    batch_size: 100,
                    ..SyncOptions::default()
                },
                200,
            )
            .unwrap();
        let records = store.inspect("", 10, true).unwrap();
        let moved = records
            .iter()
            .find(|record| record.key == "episode:100")
            .unwrap();
        assert!(!moved.deleted);
        assert_eq!(moved.metadata["parent"], "media:new");
        assert!(
            !records
                .iter()
                .find(|record| record.key == "episode:200")
                .unwrap()
                .deleted
        );
    }
}
