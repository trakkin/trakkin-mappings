use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};
use trakkin_mappings_corpus::{
    git::Repository,
    pipeline::{Action, Database, Review},
};
use trakkin_mappings_source::storage::{Batch, Commit, Postgres, Update};
use trakkin_mappings_test_support::Database as Fixture;

fn commit(store: &mut Postgres, updates: BTreeMap<String, Update>) {
    store
        .commit(Commit {
            state: &json!({}),
            batch: &Batch { updates },
            seen: &[],
            run_id: "corpus-fixture",
        })
        .unwrap();
}

fn movie(id: u64, imdb: &str) -> Update {
    Update::record(&json!({"id":id,"title":"A Film", "release_date":"2020-01-01", "external_ids":{"imdb_id":imdb}}), json!({})).unwrap()
}

#[test]
fn structural_candidates_align_ranges_in_the_declared_default_order() {
    let fixture = Fixture::start();
    let url = fixture.url.as_str();
    let mut tmdb = Postgres::connect(
        url,
        "org.themoviedb",
        &["tv", "season", "episode", "episode_group"],
    )
    .unwrap();
    tmdb.begin().unwrap();
    commit(&mut tmdb, BTreeMap::from([
        ("tv:7".into(), Update::record(&json!({"id":7,"seasons":[{"id":70,"season_number":1}],"episode_groups":{"results":[{"id":"alternate"}]}}), json!({})).unwrap()),
        ("season:70".into(), Update::record(&json!({"id":70,"season_number":1,"episodes":[
            {"id":701,"season_number":1,"episode_number":1},
            {"id":702,"season_number":1,"episode_number":2},
            {"id":703,"season_number":1,"episode_number":3},
            {"id":704,"season_number":1,"episode_number":4}
        ]}), json!({"parent":"tv:7"})).unwrap()),
        ("episode:701".into(), Update::record(&json!({"id":701,"external_ids":{"tvdb_id":801}}), json!({"parent":"tv:7"})).unwrap()),
        ("episode:702".into(), Update::record(&json!({"id":702,"external_ids":{"tvdb_id":802}}), json!({"parent":"tv:7"})).unwrap()),
        ("episode:704".into(), Update::record(&json!({"id":704,"external_ids":{"tvdb_id":804}}), json!({"parent":"tv:7"})).unwrap()),
        ("episode_group:alternate".into(), Update::record(&json!({"id":"alternate","groups":[{"order":0,"episodes":[{"id":702,"order":0},{"id":701,"order":1}]}]}), json!({"parent":"tv:7"})).unwrap()),
    ]));
    let mut tvdb = Postgres::connect(url, "com.thetvdb", &["series"]).unwrap();
    tvdb.begin().unwrap();
    let mut native = json!({
        "series":{"id":8,"defaultSeasonType":2,"seasonTypes":[{"id":1,"type":"official"},{"id":2,"type":"dvd"}],"seasons":[{"id":80,"number":2},{"id":81,"number":1}]},
        "episode_orders":{
            "official":[{"id":801,"seasonNumber":1,"number":1},{"id":802,"seasonNumber":1,"number":2},{"id":803,"seasonNumber":1,"number":3},{"id":804,"seasonNumber":1,"number":4}],
            "dvd":[{"id":801,"seasonNumber":2,"number":4},{"id":802,"seasonNumber":2,"number":5},{"id":803,"seasonNumber":2,"number":6},{"id":804,"seasonNumber":2,"number":7}]
        }
    });
    commit(
        &mut tvdb,
        BTreeMap::from([(
            "series:8".into(),
            Update::record(&native, json!({})).unwrap(),
        )]),
    );
    let mut database = Database::connect(url).unwrap();
    database.sync(1, None).unwrap();
    let parent_version = database
        .review(Review {
            record: "org.themoviedb://tv/7 <=> com.thetvdb://series/8".into(),
            action: Action::Accepted,
            note: "Verified series identity".into(),
            expected_candidate: None,
            expected_decision: None,
            replaces: None,
        })
        .unwrap();
    database.sync(1, None).unwrap();
    let structural = trakkin_mappings_corpus::parse_record(
        "org.themoviedb://tv/7 :: season=1,episode=1..2 <=> com.thetvdb://series/8 :: order=dvd,season=2,episode=4..5",
    ).unwrap();
    let shown = database
        .show(&structural.statement.claim_id().to_string(), None, 100)
        .unwrap();
    assert_eq!(shown["supports"].as_array().unwrap().len(), 1);
    assert_eq!(database.status().unwrap()["diagnostics"], json!([]));
    let separated = trakkin_mappings_corpus::parse_record(
        "org.themoviedb://tv/7 :: season=1,episode=4 <=> com.thetvdb://series/8 :: order=dvd,season=2,episode=7",
    ).unwrap();
    assert_eq!(
        database
            .show(&separated.statement.claim_id().to_string(), None, 100)
            .unwrap()["supports"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        database
            .candidates(None, 100)
            .unwrap()
            .iter()
            .all(|candidate| !candidate.record.contains("://episode/"))
    );
    assert!(
        database
            .candidates(None, 100)
            .unwrap()
            .iter()
            .all(|candidate| !candidate.record.contains("episode_group="))
    );
    let aired = trakkin_mappings_corpus::parse_record(
        "org.themoviedb://tv/7 :: season=1,episode=1..2 <=> com.thetvdb://series/8 :: order=aired,season=1,episode=1..2",
    ).unwrap();
    assert!(
        database
            .show(&aired.statement.claim_id().to_string(), None, 100)
            .is_err()
    );
    let structural_version = database
        .review(Review {
            record: structural.canonical(),
            action: Action::Accepted,
            note: "Verified default-order range alignment".into(),
            expected_candidate: shown["candidate_revision"].as_i64(),
            expected_decision: None,
            replaces: None,
        })
        .unwrap();
    database.sync(1, None).unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let repository = runtime
        .block_on(Repository::publication(
            &temporary.path().join("structural.git"),
        ))
        .unwrap();
    database.publish(&repository, &runtime, 10).unwrap();
    let published = runtime.block_on(repository.head()).unwrap();
    let changes = runtime
        .block_on(repository.changes(None, &published, None, 100))
        .unwrap();
    let artifact = changes
        .changes
        .iter()
        .filter(|change| change.path != trakkin_mappings_corpus::POLICY)
        .map(|change| {
            trakkin_mappings_corpus::Artifact::parse(&change.path, change.text.as_deref().unwrap())
                .unwrap()
        })
        .find(|artifact| artifact.record.statement.claim_id() == structural.statement.claim_id())
        .unwrap();
    let bindings = trakkin_mappings_corpus::Bindings::read(&artifact.record).unwrap();
    assert_eq!(
        bindings
            .0
            .values()
            .map(|binding| binding.units.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            vec![
                "org.themoviedb://episode/701".to_owned(),
                "org.themoviedb://episode/702".to_owned()
            ],
            vec![
                "com.thetvdb://episode/801".to_owned(),
                "com.thetvdb://episode/802".to_owned()
            ],
        ])
    );

    native["series"]["defaultSeasonType"] = json!(1);
    commit(
        &mut tvdb,
        BTreeMap::from([(
            "series:8".into(),
            Update::record(&native, json!({})).unwrap(),
        )]),
    );
    for _ in 0..32 {
        database.sync(1, Some(1)).unwrap();
        database = Database::connect(url).unwrap();
        if database.status().unwrap()["jobs"] == 0 {
            break;
        }
    }
    let retained = database
        .show(&structural.statement.claim_id().to_string(), None, 100)
        .unwrap();
    assert_eq!(retained["supports"], json!([]));
    assert_eq!(retained["decision"]["version"], structural_version);
    assert_eq!(retained["decision"]["action"], "accepted");
    assert_eq!(
        database
            .show(&aired.statement.claim_id().to_string(), None, 100)
            .unwrap()["supports"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(database.status().unwrap()["diagnostics"], json!([]));
    database.publish(&repository, &runtime, 10).unwrap();
    let head = runtime.block_on(repository.head()).unwrap();
    let changes = runtime
        .block_on(repository.changes(None, &head, None, 100))
        .unwrap();
    let artifact = changes
        .changes
        .iter()
        .filter(|change| change.path != trakkin_mappings_corpus::POLICY)
        .map(|change| {
            trakkin_mappings_corpus::Artifact::parse(&change.path, change.text.as_deref().unwrap())
                .unwrap()
        })
        .find(|artifact| artifact.record.statement.claim_id() == structural.statement.claim_id())
        .unwrap();
    assert_eq!(
        trakkin_mappings_corpus::Bindings::read(&artifact.record).unwrap(),
        bindings
    );

    let official = native["episode_orders"]
        .as_object_mut()
        .unwrap()
        .remove("official")
        .unwrap();
    commit(
        &mut tvdb,
        BTreeMap::from([(
            "series:8".into(),
            Update::record(&native, json!({})).unwrap(),
        )]),
    );
    database.sync(1, None).unwrap();
    assert_eq!(
        database
            .show(&aired.statement.claim_id().to_string(), None, 100)
            .unwrap()["supports"],
        json!([])
    );
    assert!(
        database.status().unwrap()["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| {
                diagnostic["message"]
                    .as_str()
                    .unwrap()
                    .contains("TVDB default episode order is absent")
            })
    );
    native["episode_orders"]["official"] = official;
    commit(
        &mut tvdb,
        BTreeMap::from([(
            "series:8".into(),
            Update::record(&native, json!({})).unwrap(),
        )]),
    );
    database.sync(1, None).unwrap();
    assert_eq!(database.status().unwrap()["diagnostics"], json!([]));
    database
        .review(Review {
            record: "org.themoviedb://tv/7 <=> com.thetvdb://series/8".into(),
            action: Action::Rejected,
            note: "Parent identity withdrawn".into(),
            expected_candidate: None,
            expected_decision: Some(parent_version),
            replaces: None,
        })
        .unwrap();
    database.sync(1, None).unwrap();
    assert_eq!(
        database
            .show(&aired.statement.claim_id().to_string(), None, 100)
            .unwrap()["supports"],
        json!([])
    );
}

#[test]
fn bounded_syncs_match_across_sources_while_ingestion_keeps_growing() {
    let fixture = Fixture::start();
    let url = fixture.url.as_str();
    let tvdb_movie = |id| {
        Update::record(
            &json!({"id":id,"name":"A Film","year":2020,"remoteIds":[{"sourceName":"imdb","id":format!("tt{id}")}]}),
            json!({}),
        )
        .unwrap()
    };
    let mut tvdb = Postgres::connect(url, "com.thetvdb", &["movie"]).unwrap();
    tvdb.begin().unwrap();
    commit(
        &mut tvdb,
        (1..=8)
            .map(|id| (format!("movie:{id}"), tvdb_movie(id)))
            .collect(),
    );
    let mut tmdb = Postgres::connect(url, "org.themoviedb", &["movie"]).unwrap();
    tmdb.begin().unwrap();
    commit(
        &mut tmdb,
        (1..=8)
            .map(|id| (format!("movie:{id}"), movie(id, &format!("tt{id}"))))
            .collect(),
    );
    let claim_id =
        trakkin_mappings_corpus::parse_record("org.themoviedb://movie/1 <=> com.thetvdb://movie/1")
            .unwrap()
            .statement
            .claim_id()
            .to_string();
    let mut matched = false;
    for step in 0..48 {
        let id = 100_000 + step;
        commit(
            &mut tvdb,
            BTreeMap::from([(format!("movie:{id}"), tvdb_movie(id))]),
        );
        let mut database = Database::connect(url).unwrap();
        let report = database.sync(1, Some(1)).unwrap();
        assert!(report.records + report.match_pages <= 1);
        if database
            .candidates(None, 1000)
            .unwrap()
            .iter()
            .any(|candidate| candidate.claim_id == claim_id && candidate.supports > 0)
        {
            let status = database.status().unwrap();
            assert!(status["sources"].as_array().unwrap().iter().any(|source| {
                source["source"] == "com.thetvdb" && source["bootstrap"] == true
            }));
            matched = true;
            break;
        }
    }
    assert!(
        matched,
        "matching must progress before the busy source catches up"
    );
}

#[test]
fn restored_database_recovers_pending_publication_after_git_cache_loss() {
    let fixture = Fixture::start();
    let mut source = Postgres::connect(fixture.url.as_str(), "org.themoviedb", &["movie"]).unwrap();
    source.begin().unwrap();
    commit(
        &mut source,
        BTreeMap::from([("movie:1".into(), movie(1, "tt1"))]),
    );
    let mut database = Database::connect(fixture.url.as_str()).unwrap();
    database.sync(100, None).unwrap();
    let candidate = database.candidates(None, 100).unwrap().remove(0);
    database
        .review(Review {
            record: candidate.record,
            action: Action::Accepted,
            note: "Verified mapping before backup".into(),
            expected_candidate: Some(candidate.revision),
            expected_decision: None,
            replaces: None,
        })
        .unwrap();
    let expected = database.show(&candidate.claim_id, None, 100).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("publication.git");
    let repository = runtime.block_on(Repository::publication(&path)).unwrap();
    let pending = database
        .prepare_publication(&repository, &runtime, 100)
        .unwrap()
        .unwrap();
    let archive = fixture.backup();
    drop(database);
    drop(source);
    drop(repository);
    std::fs::remove_dir_all(&path).unwrap();

    let restored = Fixture::start();
    restored.restore(&archive);
    let mut database = Database::connect(restored.url.as_str()).unwrap();
    assert_eq!(
        database.show(&candidate.claim_id, None, 100).unwrap(),
        expected
    );
    let mut source =
        Postgres::connect(restored.url.as_str(), "org.themoviedb", &["movie"]).unwrap();
    assert_eq!(source.revision().unwrap(), 1);
    assert!(source.get("movie:1").unwrap().is_some());
    let repository = runtime.block_on(Repository::publication(&path)).unwrap();
    let published = database.publish(&repository, &runtime, 100).unwrap();
    assert_eq!(published.commit, Some(pending.prepared.commit.clone()));
    assert_eq!(
        runtime.block_on(repository.head()).unwrap(),
        pending.prepared.commit
    );
    assert_eq!(
        database
            .publish(&repository, &runtime, 100)
            .unwrap()
            .batches,
        0
    );
}

#[test]
fn pipeline_rehearsal_reports_throughput_and_storage() {
    let count: u64 = std::env::var("TRAKKIN_MAPPINGS_REHEARSAL_RECORDS")
        .map(|value| value.parse().unwrap())
        .unwrap_or(25);
    assert!((1..=10_000).contains(&count));
    let fixture = Fixture::start();
    let url = fixture.url.as_str();
    let mut tmdb = Postgres::connect(url, "org.themoviedb", &["movie"]).unwrap();
    tmdb.begin().unwrap();
    let mut tvdb = Postgres::connect(url, "com.thetvdb", &["movie"]).unwrap();
    tvdb.begin().unwrap();
    let ingestion_started = Instant::now();
    for start in (1..=count).step_by(1000) {
        let end = (start + 999).min(count);
        commit(&mut tmdb, (start..=end).map(|id| {
            (format!("movie:{id}"), Update::record(
                &json!({"id":id,"title":format!("Fixture Film {id}"),"release_date":"2020-01-01","external_ids":{"imdb_id":format!("tt{id}")}}),
                json!({}),
            ).unwrap())
        }).collect());
        commit(&mut tvdb, (start..=end).map(|id| {
            (format!("movie:{id}"), Update::record(
                &json!({"id":id,"name":format!("Fixture Film {id}"),"year":2020,"remoteIds":[{"sourceName":"imdb","id":format!("tt{id}")}]}),
                json!({}),
            ).unwrap())
        }).collect());
    }
    let ingestion_seconds = ingestion_started.elapsed().as_secs_f64();
    let mut database = Database::connect(url).unwrap();
    let projection_started = Instant::now();
    let projected = database.sync(500, None).unwrap();
    let projection_seconds = projection_started.elapsed().as_secs_f64();
    assert_eq!(projected.records, count * 2);
    let mut expected = (1..=count)
        .map(|id| {
            trakkin_mappings_corpus::parse_record(&format!(
                "org.themoviedb://movie/{id} <=> com.thetvdb://movie/{id}"
            ))
            .unwrap()
            .statement
            .claim_id()
            .to_string()
        })
        .collect::<BTreeSet<_>>();
    let review_started = Instant::now();
    let mut after = None;
    loop {
        let page = database.candidates(after.as_deref(), 1000).unwrap();
        if page.is_empty() {
            break;
        }
        after = Some(page.last().unwrap().claim_id.clone());
        for candidate in page {
            if !expected.remove(&candidate.claim_id) {
                continue;
            }
            assert!(candidate.supports > 0);
            database
                .review(Review {
                    record: candidate.record,
                    action: Action::Accepted,
                    note: "Verified deterministic fixture identity".into(),
                    expected_candidate: Some(candidate.revision),
                    expected_decision: None,
                    replaces: None,
                })
                .unwrap();
        }
    }
    assert!(
        expected.is_empty(),
        "all fixture identities must be discovered"
    );
    database.sync(500, None).unwrap();
    let review_seconds = review_started.elapsed().as_secs_f64();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let repository = runtime
        .block_on(Repository::publication(
            &directory.path().join("publication.git"),
        ))
        .unwrap();
    let publication_started = Instant::now();
    let publication = database.publish(&repository, &runtime, 250).unwrap();
    let publication_seconds = publication_started.elapsed().as_secs_f64();
    assert!(publication.batches > 0);
    let status = database.status().unwrap();
    assert_eq!(status["jobs"], 0);
    assert_eq!(status["publication_pending"], 0);
    assert_eq!(database.sync(500, None).unwrap().records, 0);
    let mut inspection = postgres::Client::connect(url, postgres::NoTls).unwrap();
    let sizes = inspection.query_one("SELECT pg_database_size(current_database()),pg_total_relation_size('ingestion.record_versions'),pg_total_relation_size('corpus.records')", &[]).unwrap();
    let backup_bytes = fixture.backup().len();
    println!(
        "{}",
        json!({
            "fixture":"synthetic-movie-pairs",
            "profile":if cfg!(debug_assertions) { "debug" } else { "release" },
            "records_per_source":count,
            "ingestion":{"seconds":ingestion_seconds,"records_per_second":(count * 2) as f64 / ingestion_seconds},
            "corpus":{"seconds":projection_seconds,"records_per_second":projected.records as f64 / projection_seconds,"match_pages":projected.match_pages},
            "review":{"seconds":review_seconds,"claims":count},
            "publication":{"seconds":publication_seconds,"batches":publication.batches},
            "storage":{"database_bytes":sizes.get::<_,i64>(0),"source_versions_bytes":sizes.get::<_,i64>(1),"projection_bytes":sizes.get::<_,i64>(2),"backup_bytes":backup_bytes}
        })
    );
}

#[test]
fn corpus_recovers_bootstrap_replays_and_retracts_dependent_evidence() {
    let fixture = Fixture::start();
    let url = &fixture.url;
    let mut source = Postgres::connect(url.as_str(), "org.themoviedb", &["movie"]).unwrap();
    source.begin().unwrap();
    commit(
        &mut source,
        BTreeMap::from([
            ("movie:20".into(), movie(20, "tt20")),
            ("movie:30".into(), movie(30, "tt30")),
        ]),
    );
    let mut database = Database::connect(url.as_str()).unwrap();
    database.sync(1, Some(1)).unwrap();
    assert_eq!(database.status().unwrap()["sources"][0]["bootstrap"], true);
    commit(
        &mut source,
        BTreeMap::from([
            ("movie:1".into(), movie(1, "tt1")),
            ("movie:20".into(), movie(20, "tt200")),
        ]),
    );
    database.sync(1, None).unwrap();
    let status = database.status().unwrap();
    assert_eq!(status["records"], 3);
    assert_eq!(status["sources"][0]["revision"], source.revision().unwrap());
    assert_eq!(status["sources"][0]["bootstrap"], false);
    let candidates = database.candidates(None, 100).unwrap();
    let old = candidates
        .iter()
        .find(|candidate| candidate.record.contains("title/tt20 "))
        .unwrap();
    assert_eq!(old.supports, 0);
    let current = candidates
        .iter()
        .find(|candidate| candidate.record.contains("title/tt200 "))
        .unwrap();
    assert_eq!(current.supports, 1);
    let before: Value = database.show(&current.claim_id, None, 1000).unwrap();
    assert!(database.show(&current.claim_id, None, 0).is_err());
    let first_support = before["supports"][0]["id"].as_str().unwrap();
    assert_eq!(
        database
            .show(&current.claim_id, Some(first_support), 1)
            .unwrap()["supports"],
        json!([])
    );
    assert_eq!(database.sync(1, None).unwrap().records, 0);
    assert_eq!(
        database.show(&current.claim_id, None, 1000).unwrap(),
        before
    );
    let decision = database
        .review(Review {
            record: current.record.clone(),
            action: Action::Accepted,
            note: "Verified native external identifier".into(),
            expected_candidate: Some(current.revision),
            expected_decision: None,
            replaces: None,
        })
        .unwrap();
    assert!(
        database
            .review(Review {
                record: current.record.clone(),
                action: Action::Rejected,
                note: "Stale decision edit".into(),
                expected_candidate: None,
                expected_decision: None,
                replaces: None
            })
            .is_err()
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let repository = runtime
        .block_on(Repository::publication(
            &directory.path().join("publication.git"),
        ))
        .unwrap();
    let pending = database
        .prepare_publication(&repository, &runtime, 10)
        .unwrap()
        .unwrap();
    runtime
        .block_on(repository.apply(&pending.prepared))
        .unwrap();
    drop(database);
    let mut database = Database::connect(url.as_str()).unwrap();
    let published = database.publish(&repository, &runtime, 10).unwrap();
    assert_eq!(
        published.commit.as_deref(),
        Some(pending.prepared.commit.as_str())
    );
    assert_eq!(
        runtime.block_on(repository.head()).unwrap(),
        pending.prepared.commit
    );
    assert_eq!(
        database.publish(&repository, &runtime, 10).unwrap().batches,
        0
    );
    commit(
        &mut source,
        BTreeMap::from([(
            "movie:20".into(),
            Update::deletion(&json!({}), json!({})).unwrap(),
        )]),
    );
    database.sync(1, None).unwrap();
    assert_eq!(
        database.show(&current.claim_id, None, 1000).unwrap()["supports"],
        json!([])
    );
    assert_eq!(
        database.show(&current.claim_id, None, 1000).unwrap()["decision"]["version"],
        decision
    );
    assert_eq!(
        database.show(&current.claim_id, None, 1000).unwrap()["decision"]["action"],
        "accepted"
    );
    drop(source);
    let mut tmdb =
        Postgres::connect(url.as_str(), "org.themoviedb", &["movie", "tv", "season"]).unwrap();
    tmdb.begin().unwrap();
    commit(&mut tmdb,BTreeMap::from([
        ("tv:7".into(),Update::record(&json!({"id":7,"seasons":[{"id":70,"season_number":1}],"episode_groups":{"results":[]}}),json!({})).unwrap()),
        ("season:70".into(),Update::record(&json!({"id":70,"season_number":1,"episodes":[{"id":701,"season_number":1,"episode_number":1,"name":"Arrival","air_date":"2020-01-01"}]}),json!({"parent":"tv:7"})).unwrap()),
        ("movie:30".into(),movie(30,"tt300")),
    ]));
    let mut tvdb = Postgres::connect(url.as_str(), "com.thetvdb", &["series"]).unwrap();
    tvdb.begin().unwrap();
    commit(&mut tvdb,BTreeMap::from([("series:8".into(),Update::record(&json!({"series":{"id":8,"seasonTypes":[{"id":1,"type":"official"}],"defaultSeasonType":1,"seasons":[{"id":80,"number":1}]},"episode_orders":{"official":[{"id":801,"seasonNumber":1,"number":1,"name":"Arrival","aired":"2020-01-01"}]}}),json!({})).unwrap())]));
    let mut anilist = Postgres::connect(url.as_str(), "co.anilist", &["media"]).unwrap();
    anilist.begin().unwrap();
    commit(
        &mut anilist,
        BTreeMap::from([(
            "media:1".into(),
            Update::record(&json!({"id":1,"type":"ANIME","episodes":2}), json!({})).unwrap(),
        )]),
    );
    database.sync(2, None).unwrap();
    let parent = "org.themoviedb://tv/7 <=> com.thetvdb://series/8";
    let parent_version = database
        .review(Review {
            record: parent.into(),
            action: Action::Accepted,
            note: "Verified parent identity".into(),
            expected_candidate: None,
            expected_decision: None,
            replaces: None,
        })
        .unwrap();
    database.sync(1, None).unwrap();
    let episode_id = trakkin_mappings_corpus::parse_record(
        "org.themoviedb://tv/7 :: season=1,episode=1 <=> com.thetvdb://series/8 :: order=aired,season=1,episode=1",
    )
    .unwrap()
    .statement
    .claim_id()
    .to_string();
    assert_eq!(
        database.show(&episode_id, None, 1000).unwrap()["supports"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    database
        .review(Review {
            record: parent.into(),
            action: Action::Rejected,
            note: "Parent identity was disproven".into(),
            expected_candidate: None,
            expected_decision: Some(parent_version),
            replaces: None,
        })
        .unwrap();
    database.sync(1, None).unwrap();
    assert_eq!(
        database.show(&episode_id, None, 1000).unwrap()["supports"],
        json!([])
    );
    let selector = "co.anilist://media/1 :: episode=* <~> [x://1,x://2]";
    let selector_id = trakkin_mappings_corpus::parse_record(selector)
        .unwrap()
        .statement
        .claim_id();
    database
        .review(Review {
            record: selector.into(),
            action: Action::Accepted,
            note: "Reviewed native coverage".into(),
            expected_candidate: None,
            expected_decision: None,
            replaces: None,
        })
        .unwrap();
    database.sync(2, None).unwrap();
    database.publish(&repository, &runtime, 2).unwrap();
    let before = runtime.block_on(repository.head()).unwrap();
    commit(
        &mut anilist,
        BTreeMap::from([(
            "media:1".into(),
            Update::record(&json!({"id":1,"type":"ANIME","episodes":3}), json!({})).unwrap(),
        )]),
    );
    database.sync(2, None).unwrap();
    database.publish(&repository, &runtime, 2).unwrap();
    let after = runtime.block_on(repository.head()).unwrap();
    let delta = runtime
        .block_on(repository.changes(Some(&before), &after, None, 1000))
        .unwrap();
    assert_eq!(delta.changes.len(), 1);
    let artifact = trakkin_mappings_corpus::Artifact::parse(
        &delta.changes[0].path,
        delta.changes[0].text.as_deref().unwrap(),
    )
    .unwrap();
    assert_eq!(artifact.record.statement.claim_id(), selector_id);
    assert_eq!(
        trakkin_mappings_corpus::Bindings::read(&artifact.record)
            .unwrap()
            .0
            .values()
            .next()
            .unwrap()
            .units
            .len(),
        3
    );
    let policy=trakkin_mappings_corpus::ConflictPolicy::parse(r#"{"format_version":1,"exclusive_source_pairs":[{"origin_source":"a","target_source":"b"}]}"#).unwrap();
    database.set_policy(&policy).unwrap();
    database.sync(2, None).unwrap();
    let first = "a://1 <=> b://1";
    let first_id = trakkin_mappings_corpus::parse_record(first)
        .unwrap()
        .statement
        .claim_id()
        .to_string();
    let first_version = database
        .review(Review {
            record: first.into(),
            action: Action::Accepted,
            note: "Original reviewed identity".into(),
            expected_candidate: None,
            expected_decision: None,
            replaces: None,
        })
        .unwrap();
    let replacement = "a://1 <=> b://2";
    assert!(
        database
            .review(Review {
                record: replacement.into(),
                action: Action::Accepted,
                note: "Would violate explicit exclusivity".into(),
                expected_candidate: None,
                expected_decision: None,
                replaces: None
            })
            .is_err()
    );
    database
        .review(Review {
            record: replacement.into(),
            action: Action::Accepted,
            note: "Correct the original identity".into(),
            expected_candidate: None,
            expected_decision: None,
            replaces: Some((first_id.clone(), first_version)),
        })
        .unwrap();
    assert_eq!(
        database.show(&first_id, None, 1000).unwrap()["decision"]["action"],
        "rejected"
    );
    let invalidated = "co.anilist://media/1 :: episode=* <=> [x://1,x://2,x://3]";
    let invalidated_id = trakkin_mappings_corpus::parse_record(invalidated)
        .unwrap()
        .statement
        .claim_id()
        .to_string();
    let invalidated_version = database
        .review(Review {
            record: invalidated.into(),
            action: Action::Accepted,
            note: "Three-episode ordered alignment".into(),
            expected_candidate: None,
            expected_decision: None,
            replaces: None,
        })
        .unwrap();
    commit(
        &mut anilist,
        BTreeMap::from([(
            "media:1".into(),
            Update::record(&json!({"id":1,"type":"ANIME","episodes":4}), json!({})).unwrap(),
        )]),
    );
    assert!(
        database
            .review(Review {
                record: "co.anilist://media/1 :: episode=* <~> y://1".into(),
                action: Action::Accepted,
                note: "Reject lagging selector projection".into(),
                expected_candidate: None,
                expected_decision: None,
                replaces: None
            })
            .is_err()
    );
    database.sync(2, None).unwrap();
    assert!(
        database
            .prepare_publication(&repository, &runtime, 100)
            .unwrap_err()
            .to_string()
            .contains(&invalidated_id)
    );
    database
        .review(Review {
            record: invalidated.into(),
            action: Action::Rejected,
            note: "Native extent disproved ordered alignment".into(),
            expected_candidate: None,
            expected_decision: Some(invalidated_version),
            replaces: None,
        })
        .unwrap();
    database.publish(&repository, &runtime, 100).unwrap();
    let mut inspection = postgres::Client::connect(url.as_str(), postgres::NoTls).unwrap();
    inspection
        .batch_execute(
            "UPDATE ingestion.record_versions SET recorded_at=clock_timestamp()-interval '40 days'",
        )
        .unwrap();
    let retained = database.retain(30, 1, 1000).unwrap();
    assert!(retained.versions > 0);
    assert_eq!(inspection.query_one("SELECT count(*) FROM ingestion.record_versions WHERE source='org.themoviedb' AND key='movie:20' AND revision=2",&[]).unwrap().get::<_,i64>(0),1);
    assert_eq!(inspection.query_one("SELECT count(*) FROM ingestion.record_versions WHERE source='co.anilist' AND key='media:1' AND revision=1",&[]).unwrap().get::<_,i64>(0),1);
    drop(inspection);
    drop(database);
    drop(tmdb);
    drop(tvdb);
    drop(anilist);
}

#[test]
fn corpus_discovers_shared_identifier_and_title_year_matches() {
    let fixture = Fixture::start();
    let url = fixture.url.as_str();
    let mut tmdb = Postgres::connect(url, "org.themoviedb", &["movie"]).unwrap();
    tmdb.begin().unwrap();
    commit(
        &mut tmdb,
        BTreeMap::from([("movie:1".into(), movie(1, "tt1"))]),
    );
    let mut tvdb = Postgres::connect(url, "com.thetvdb", &["movie"]).unwrap();
    tvdb.begin().unwrap();
    commit(
        &mut tvdb,
        BTreeMap::from([(
            "movie:1".into(),
            Update::record(
                &json!({"id":1,"name":"A Film","year":2020,"remoteIds":[{"sourceName":"imdb","id":"tt1"}]}),
                json!({}),
            ).unwrap(),
        )]),
    );
    let mut database = Database::connect(url).unwrap();
    database.sync(1, None).unwrap();
    let claim_id =
        trakkin_mappings_corpus::parse_record("org.themoviedb://movie/1 <=> com.thetvdb://movie/1")
            .unwrap()
            .statement
            .claim_id()
            .to_string();
    let candidate = database
        .candidates(None, 100)
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.claim_id == claim_id)
        .unwrap();
    assert_eq!(candidate.supports, 2);
    let shown = database.show(&claim_id, None, 100).unwrap();
    let supports = shown["supports"].as_array().unwrap();
    assert_eq!(supports.len(), 2);
    for method in ["shared-identifier", "title-year"] {
        let evidence = supports
            .iter()
            .find(|support| support["method"] == method)
            .unwrap()["evidence"]
            .as_array()
            .unwrap();
        assert_eq!(evidence.len(), 2);
        assert_ne!(evidence[0]["source"], evidence[1]["source"]);
        for input in evidence {
            assert_eq!(input["key"], "movie:1");
            assert_eq!(input["revision"], 1);
        }
    }
}

#[test]
fn corpus_bounds_ambiguous_blocks_and_resumes_engine_reprojection() {
    let fixture = Fixture::start();
    let url = fixture.url.as_str();
    let mut tmdb = Postgres::connect(url, "org.themoviedb", &["movie"]).unwrap();
    tmdb.begin().unwrap();
    commit(
        &mut tmdb,
        (1..=260)
            .map(|id| (format!("movie:{id}"), movie(id, &format!("tt{id}"))))
            .collect(),
    );
    let mut tvdb = Postgres::connect(url, "com.thetvdb", &["movie"]).unwrap();
    tvdb.begin().unwrap();
    commit(
        &mut tvdb,
        (1..=260)
            .map(|id| {
                (
                    format!("movie:{id}"),
                    Update::record(&json!({"id":id,"name":"A Film","year":2020}), json!({}))
                        .unwrap(),
                )
            })
            .collect(),
    );
    let mut database = Database::connect(url).unwrap();
    let first = database.sync(17, Some(1)).unwrap();
    assert_eq!(first.records, 17);
    assert_eq!(first.match_pages, 0);
    database.sync(17, None).unwrap();
    let status = database.status().unwrap();
    assert_eq!(status["records"], 520);
    assert_eq!(status["jobs"], 0);
    assert_eq!(status["diagnostics"].as_array().unwrap().len(), 1);
    assert!(
        status["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("256")
    );
    let mut inspection = postgres::Client::connect(url, postgres::NoTls).unwrap();
    assert_eq!(
        inspection
            .query_one(
                "SELECT count(*) FROM corpus.supports WHERE method='title-year'",
                &[]
            )
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    let stale_id = trakkin_mappings_corpus::parse_record(
        "org.themoviedb://movie/260 <=> com.imdb://title/tt260",
    )
    .unwrap()
    .statement
    .claim_id()
    .to_string();
    let stale = database
        .candidates(None, 1000)
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.claim_id == stale_id)
        .unwrap();
    commit(
        &mut tmdb,
        BTreeMap::from([("movie:260".into(), movie(260, "tt999"))]),
    );
    assert!(
        database
            .review(Review {
                record: stale.record,
                action: Action::Accepted,
                note: "Stale raw dependency must not pass review".into(),
                expected_candidate: Some(stale.revision),
                expected_decision: None,
                replaces: None
            })
            .is_err()
    );
    database.sync(17, None).unwrap();
    database.set_policy(&trakkin_mappings_corpus::ConflictPolicy::parse(r#"{"format_version":1,"exclusive_source_pairs":[{"origin_source":"manual","target_source":"other"}]}"#).unwrap()).unwrap();
    database.sync(17, None).unwrap();
    database
        .review(Review {
            record: "manual://1 <=> other://1".into(),
            action: Action::Accepted,
            note: "Authored without catalog inputs".into(),
            expected_candidate: None,
            expected_decision: None,
            replaces: None,
        })
        .unwrap();
    database.sync(17, None).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let repository = runtime
        .block_on(Repository::publication(
            &directory.path().join("publication.git"),
        ))
        .unwrap();
    database.publish(&repository, &runtime, 100).unwrap();
    inspection
        .batch_execute("DELETE FROM corpus.footprints WHERE origin='manual://1'")
        .unwrap();
    inspection
        .batch_execute("UPDATE corpus.engine SET revision='previous-engine'")
        .unwrap();
    let upgrade = database.sync(17, Some(1)).unwrap();
    assert_eq!(upgrade.records, 17);
    assert_eq!(database.status().unwrap()["sources"][0]["bootstrap"], true);
    assert_eq!(database.status().unwrap()["publication_pending"], 1);
    database.sync(17, None).unwrap();
    assert_eq!(inspection.query_one("SELECT count(*) FROM corpus.footprints WHERE origin='manual://1' AND target_source='other' AND target='other://1'",&[]).unwrap().get::<_,i64>(0),1);
    assert_eq!(database.status().unwrap()["records"], 520);
    assert_eq!(database.sync(17, None).unwrap().records, 0);
    assert_eq!(
        database.show(&stale_id, None, 1000).unwrap()["supports"],
        json!([])
    );
}
