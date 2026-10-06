mod common;

use common::{Reply, reply, server};
use flate2::{Compression, write::GzEncoder};
use serde_json::{Value, json};
use std::io::Write;
use trakkin_mappings_ingestion::{Change, Provider, providers::Tmdb};

#[test]
fn shared_hierarchy_acquires_show_and_season_once_for_all_outputs() {
    let (endpoint, handle) = server(vec![
        reply(
            "/tv/changes?page=1",
            json!({"results":[{"id":1}],"total_pages":1}),
        ),
        reply(
            "/tv/1?",
            json!({"id":1,"seasons":[{"id":10,"season_number":0}]}),
        ),
        reply("/tv/1/episode_groups", json!({"results":[{"id":"group1"}]})),
        reply(
            "/tv/1/season/0?",
            json!({"id":10,"season_number":0,"episodes":[{"id":100,"episode_number":1,"season_number":0,"show_id":1}]}),
        ),
        reply(
            "/tv/1/season/0/episode/1?",
            json!({"id":100,"episode_number":1,"season_number":0,"external_ids":{"tvdb_id":12}}),
        ),
        reply(
            "/tv/episode_group/group1",
            json!({"id":"group1","groups":[]}),
        ),
    ]);
    let mut provider = Tmdb::with_endpoints("fixture".into(), &endpoint, &endpoint).unwrap();
    provider.select_domains(&["tv", "season", "episode", "episode_group"]);
    let page = provider
        .discover_changes(100000, &Value::Null, 200000)
        .unwrap();
    assert!(page.next.is_none());
    assert_eq!(provider.fetch("tv:1").unwrap().unwrap()["id"], 1);
    assert_eq!(provider.fetch("season:10").unwrap().unwrap()["id"], 10);
    assert_eq!(
        provider.fetch("episode:100").unwrap().unwrap()["external_ids"]["tvdb_id"],
        12
    );
    assert_eq!(
        provider.fetch("episode_group:group1").unwrap().unwrap()["id"],
        "group1"
    );
    let members = page
        .changes
        .iter()
        .find_map(|change| match change {
            Change::Membership { keys, .. } => Some(keys),
            _ => None,
        })
        .unwrap();
    assert_eq!(members.len(), 3);
    assert!(
        members.contains("season:10")
            && members.contains("episode:100")
            && members.contains("episode_group:group1")
    );
    handle.join().unwrap();
}

#[test]
fn incomplete_hierarchy_never_emits_membership_or_records() {
    let (endpoint, handle) = server(vec![
        reply(
            "/tv/changes?page=1",
            json!({"results":[{"id":1}],"total_pages":1}),
        ),
        reply(
            "/tv/1?",
            json!({"id":1,"seasons":[{"id":10,"season_number":0}]}),
        ),
        reply(
            "/tv/1/season/0?",
            json!({"id":11,"season_number":0,"episodes":[]}),
        ),
    ]);
    let mut provider = Tmdb::with_endpoints("fixture".into(), &endpoint, &endpoint).unwrap();
    provider.select_domains(&["tv", "season", "episode"]);
    assert!(
        provider
            .discover_changes(100000, &Value::Null, 200000)
            .is_err()
    );
    handle.join().unwrap();
}

#[test]
fn scoped_titles_read_only_their_exports_and_change_feed() {
    for (domain, exports, change_path) in [
        (
            "movie",
            ["movie_ids_01_02_1970", "adult_movie_ids_01_02_1970"],
            "/movie/changes",
        ),
        (
            "tv",
            ["tv_series_ids_01_02_1970", "adult_tv_series_ids_01_02_1970"],
            "/tv/changes",
        ),
    ] {
        let mut replies: Vec<_> = exports
            .into_iter()
            .map(|expected| {
                let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
                writeln!(gzip, "{{\"id\":1}}").unwrap();
                Reply {
                    status: 200,
                    body: gzip.finish().unwrap(),
                    expected,
                }
            })
            .collect();
        replies.push(reply(
            change_path,
            json!({"results":[{"id":1}],"total_pages":1}),
        ));
        let (endpoint, handle) = server(replies);
        let mut provider = Tmdb::with_endpoints("fixture".into(), &endpoint, &endpoint).unwrap();
        provider.select_domain(domain);
        let page = provider.enumerate(&Value::Null, 172800).unwrap();
        assert!(matches!(&page.changes[0], Change::Dirty(key) if key == &format!("{domain}:1")));
        let page = provider.enumerate(&page.next.unwrap(), 172800).unwrap();
        assert!(page.next.is_none());
        assert!(
            provider
                .discover_changes(100000, &Value::Null, 200000)
                .unwrap()
                .next
                .is_none()
        );
        handle.join().unwrap();
    }
}

#[test]
fn season_catalogue_traverses_show_exports_and_fetches_native_payloads() {
    let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
    writeln!(gzip, "{{\"id\":1}}").unwrap();
    let empty = GzEncoder::new(Vec::new(), Compression::default())
        .finish()
        .unwrap();
    let (endpoint, handle) = server(vec![
        Reply {
            status: 200,
            body: gzip.finish().unwrap(),
            expected: "tv_series_ids_01_02_1970",
        },
        reply(
            "/tv/1?",
            json!({"id":1,"seasons":[{"id":10,"season_number":0}]}),
        ),
        reply(
            "/tv/1/season/0?",
            json!({"id":10,"season_number":0,"episodes":[]}),
        ),
        Reply {
            status: 200,
            body: empty,
            expected: "adult_tv_series_ids_01_02_1970",
        },
    ]);
    let provider = Tmdb::with_endpoints("fixture".into(), &endpoint, &endpoint).unwrap();
    let mut provider =
        trakkin_mappings_ingestion::dataset::AcquisitionPlan::new(Box::new(provider), &["season"])
            .unwrap();
    let page = provider.enumerate(&Value::Null, 172800).unwrap();
    assert!(matches!(&page.changes[0], Change::Dirty(key) if key == "season:10"));
    assert_eq!(provider.fetch("season:10").unwrap().unwrap()["id"], 10);
    let page = provider.enumerate(&page.next.unwrap(), 172800).unwrap();
    let page = provider.enumerate(&page.next.unwrap(), 172800).unwrap();
    assert!(page.next.is_none());
    handle.join().unwrap();
}

#[test]
fn episode_sync_discovers_specials_and_fetches_independent_details() {
    let (endpoint, handle) = server(vec![
        reply(
            "/tv/changes?page=1",
            json!({"results":[{"id":1}],"total_pages":1}),
        ),
        reply(
            "/tv/1?",
            json!({"id":1,"seasons":[{"id":10,"season_number":0}]}),
        ),
        reply(
            "/tv/1/season/0?",
            json!({"id":10,"season_number":0,"episodes":[{"id":100,"episode_number":1,"season_number":0,"show_id":1}]}),
        ),
        reply(
            "/tv/1/season/0/episode/1?",
            json!({"id":100,"episode_number":1,"season_number":0,"external_ids":{"tvdb_id":12}}),
        ),
    ]);
    let provider = Tmdb::with_endpoints("fixture".into(), &endpoint, &endpoint).unwrap();
    let mut provider =
        trakkin_mappings_ingestion::dataset::AcquisitionPlan::new(Box::new(provider), &["episode"])
            .unwrap();
    let page = provider
        .discover_changes(100000, &Value::Null, 200000)
        .unwrap();
    assert!(page.next.is_none());
    assert!(matches!(&page.changes[0], Change::Dirty(key) if key == "episode:100"));
    assert_eq!(provider.metadata("episode:100")["address"], "episode:1/0/1");
    assert_eq!(
        provider.fetch("episode:100").unwrap().unwrap()["external_ids"]["tvdb_id"],
        12
    );
    assert!(provider.fetch("1/../1").is_err());
    handle.join().unwrap();
}

#[test]
fn detail_requests_overlap_and_share_pacing() {
    use std::{
        net::TcpListener,
        thread,
        time::{Duration, Instant},
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        let first_request = common::read_request(&mut first);
        let started = Instant::now();
        let (mut second, _) = listener.accept().unwrap();
        let second_request = common::read_request(&mut second);
        assert!(started.elapsed() >= Duration::from_millis(20));
        for (stream, request) in [(&mut first, first_request), (&mut second, second_request)] {
            let media_id = if request.contains("/movie/1?") { 1 } else { 2 };
            common::write_reply(stream, reply("", json!({"id":media_id})), "0");
        }
    });
    let mut provider = Tmdb::with_endpoints("fixture".into(), &endpoint, &endpoint).unwrap();
    let records = provider
        .fetch_many(&["movie:1".into(), "movie:2".into()])
        .unwrap();
    assert_eq!(records.len(), 2);
    handle.join().unwrap();
}

#[test]
fn episode_groups_preserve_nested_ordering_and_membership() {
    let (endpoint, handle) = server(vec![
        reply(
            "/tv/changes?page=1",
            json!({"results":[{"id":1}],"total_pages":1}),
        ),
        reply("/tv/1?", json!({"id":1})),
        reply("/tv/1/episode_groups", json!({"results":[{"id":"abc123"}]})),
        reply(
            "/tv/episode_group/abc123",
            json!({"id":"abc123","type":3,"groups":[{"id":"def456","order":0,"episodes":[{"id":100,"order":1},{"id":101,"order":0}]}]}),
        ),
    ]);
    let provider = Tmdb::with_endpoints("fixture".into(), &endpoint, &endpoint).unwrap();
    let mut provider = trakkin_mappings_ingestion::dataset::AcquisitionPlan::new(
        Box::new(provider),
        &["episode_group"],
    )
    .unwrap();
    let page = provider
        .discover_changes(100000, &Value::Null, 200000)
        .unwrap();
    assert!(matches!(&page.changes[0], Change::Dirty(key) if key == "episode_group:abc123"));
    assert!(
        matches!(&page.changes[1], Change::Membership { parent, keys } if parent == "tv:1" && keys.contains("episode_group:abc123"))
    );
    let record = provider.fetch("episode_group:abc123").unwrap().unwrap();
    assert_eq!(record["groups"][0]["episodes"][1]["order"], 0);
    assert_eq!(provider.metadata("episode_group:abc123")["parent"], "tv:1");
    handle.join().unwrap();
}

#[test]
fn renumbering_keeps_identity_and_replaces_restored_address() {
    let (endpoint, handle) = server(vec![
        reply(
            "/tv/changes?page=1",
            json!({"results":[{"id":1}],"total_pages":1}),
        ),
        reply(
            "/tv/1?",
            json!({"id":1,"seasons":[{"id":20,"season_number":2}]}),
        ),
        reply(
            "/tv/1/season/2?",
            json!({"id":20,"season_number":2,"episodes":[{"id":100,"show_id":1,"season_number":2,"episode_number":1}]}),
        ),
        reply(
            "/tv/1/season/2/episode/1?",
            json!({"id":100,"season_number":2,"episode_number":1}),
        ),
    ]);
    let provider = Tmdb::with_endpoints("fixture".into(), &endpoint, &endpoint).unwrap();
    let mut provider =
        trakkin_mappings_ingestion::dataset::AcquisitionPlan::new(Box::new(provider), &["episode"])
            .unwrap();
    provider
        .restore(&std::collections::BTreeMap::from([(
            "episode:100".into(),
            trakkin_mappings_ingestion::storage::Entry {
                hash: "old".into(),
                deleted: false,
                metadata: json!({"parent":"tv:1","address":"episode:1/1/3"}),
            },
        )]))
        .unwrap();
    let page = provider
        .discover_changes(100000, &Value::Null, 200000)
        .unwrap();
    assert!(matches!(&page.changes[0], Change::Dirty(key) if key == "episode:100"));
    assert_eq!(provider.metadata("episode:100")["address"], "episode:1/2/1");
    assert_eq!(provider.fetch("episode:100").unwrap().unwrap()["id"], 100);
    handle.join().unwrap();
}

#[test]
fn rate_limit_cooldown_pauses_other_workers() {
    use std::{
        net::TcpListener,
        thread,
        time::{Duration, Instant},
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        common::read_request(&mut first);
        common::write_reply(
            &mut first,
            Reply {
                status: 429,
                body: vec![],
                expected: "",
            },
            "1",
        );
        let limited = Instant::now();
        drop(first);
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = common::read_request(&mut stream);
            assert!(limited.elapsed() >= Duration::from_millis(950));
            let media_id = if request.contains("/movie/1?") { 1 } else { 2 };
            common::write_reply(&mut stream, reply("", json!({"id":media_id})), "0");
        }
    });
    let mut provider = Tmdb::with_endpoints("fixture".into(), &endpoint, &endpoint).unwrap();
    assert_eq!(
        provider
            .fetch_many(&["movie:1".into(), "movie:2".into()])
            .unwrap()
            .len(),
        2
    );
    handle.join().unwrap();
}

#[test]
fn failed_batch_never_returns_partial_records() {
    let (endpoint, handle) = server(vec![Reply {
        status: 401,
        body: vec![],
        expected: "/movie/1",
    }]);
    let mut provider = Tmdb::with_endpoints("fixture".into(), &endpoint, &endpoint).unwrap();
    assert!(provider.fetch_many(&["movie:1".into()]).is_err());
    handle.join().unwrap();
    let (endpoint, handle) = server(vec![reply("/movie/1", json!({"id":999}))]);
    let mut provider = Tmdb::with_endpoints("fixture".into(), &endpoint, &endpoint).unwrap();
    assert!(provider.fetch_many(&["movie:1".into()]).is_err());
    handle.join().unwrap();
}

#[test]
fn follows_change_pages_and_preserves_record_types() {
    let (endpoint, handle) = server(vec![
        reply(
            "/movie/changes?page=1",
            json!({"results":[{"id":1}],"total_pages":2}),
        ),
        reply(
            "/movie/changes?page=2",
            json!({"results":[{"id":2}],"total_pages":2}),
        ),
        reply(
            "/tv/changes?page=1",
            json!({"results":[{"id":1}],"total_pages":1}),
        ),
        reply(
            "/movie/1?append_to_response=external_ids",
            json!({"id":1,"external_ids":{"imdb_id":"tt1"},"native_field":true}),
        ),
    ]);
    let mut provider = Tmdb::with_endpoints("fixture".into(), &endpoint, &endpoint).unwrap();
    let first = provider
        .discover_changes(172800, &Value::Null, 259200)
        .unwrap();
    assert!(matches!(&first.changes[0],Change::Dirty(key) if key == "movie:1"));
    let second = provider
        .discover_changes(172800, &first.next.unwrap(), 259200)
        .unwrap();
    let third = provider
        .discover_changes(172800, &second.next.unwrap(), 259200)
        .unwrap();
    assert!(matches!(&third.changes[0],Change::Dirty(key) if key == "tv:1"));
    assert!(third.next.is_none());
    assert_eq!(
        provider.fetch("movie:1").unwrap().unwrap()["native_field"],
        true
    );
    handle.join().unwrap();
}

#[test]
fn catalogue_includes_all_four_gzip_exports() {
    let replies = [
        "movie_ids_01_02_1970",
        "tv_series_ids_01_02_1970",
        "adult_movie_ids_01_02_1970",
        "adult_tv_series_ids_01_02_1970",
    ]
    .into_iter()
    .enumerate()
    .map(|(number, expected)| {
        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        writeln!(gzip, "{{\"id\":{}}}", number + 1).unwrap();
        Reply {
            status: 200,
            body: gzip.finish().unwrap(),
            expected,
        }
    })
    .collect();
    let (endpoint, handle) = server(replies);
    let mut provider = Tmdb::with_endpoints("fixture".into(), &endpoint, &endpoint).unwrap();
    let mut cursor = Value::Null;
    let mut keys = Vec::new();
    loop {
        let page = provider.enumerate(&cursor, 172800).unwrap();
        keys.extend(page.changes.into_iter().map(|change| match change {
            Change::Dirty(key) => key,
            _ => unreachable!(),
        }));
        if let Some(next) = page.next {
            cursor = next;
        } else {
            break;
        }
    }
    assert_eq!(keys, vec!["movie:1", "tv:2", "movie:3", "tv:4"]);
    handle.join().unwrap();
}

#[test]
fn retries_rate_limits_but_never_treats_auth_failure_as_deletion() {
    let (endpoint, handle) = server(vec![
        Reply {
            status: 429,
            body: vec![],
            expected: "/movie/1",
        },
        reply("/movie/1", json!({"id":1})),
        Reply {
            status: 401,
            body: vec![],
            expected: "/movie/2",
        },
        Reply {
            status: 404,
            body: vec![],
            expected: "/movie/3",
        },
    ]);
    let mut provider = Tmdb::with_endpoints("fixture".into(), &endpoint, &endpoint).unwrap();
    assert!(provider.fetch("movie:1").unwrap().is_some());
    assert!(provider.fetch("movie:2").is_err());
    assert!(provider.fetch("movie:3").unwrap().is_none());
    handle.join().unwrap();
}
