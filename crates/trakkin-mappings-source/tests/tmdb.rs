mod common;

use common::{Reply, reply, server};
use flate2::{Compression, write::GzEncoder};
use serde_json::{Value, json};
use std::{collections::BTreeSet, io::Write};
use trakkin_mappings_source::{
    Change, FetchRequest, Mode, Source, SyncOptions, sources::Tmdb, status, storage::Postgres, sync,
};
use trakkin_mappings_test_support::Database;

#[test]
fn full_sync_resumes_export_offset_and_original_date_after_reopening() {
    let database = Database::start();
    let domains = ["tv", "season"];
    let options = SyncOptions {
        batch_size: 1,
        ..SyncOptions::default()
    };
    let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
    for record_id in [1, 2] {
        writeln!(gzip, "{}", json!({"id":record_id})).unwrap();
    }
    let export = gzip.finish().unwrap();
    let (endpoint, handle) = server(vec![
        Reply {
            status: 200,
            body: export.clone(),
            expected: "tv_series_ids_01_02_1970.json.gz",
        },
        reply(
            "/tv/1?",
            json!({"id":1,"seasons":[{"id":10,"season_number":1}]}),
        ),
        reply("/tv/1/season/1?", json!({"id":10,"season_number":1})),
        reply("/tv/2?", json!({"id":999})),
    ]);
    let mut provider =
        Tmdb::with_endpoints("fixture".into(), &domains, &endpoint, &endpoint).unwrap();
    let mut store = Postgres::connect(database.url.as_str(), "org.themoviedb", &domains).unwrap();
    let error = sync(&mut provider, &mut store, options, 172800, |_, _| {}).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("TMDB returned a different record")
    );
    handle.join().unwrap();
    let pending = status(&mut store)
        .unwrap()
        .scopes
        .into_iter()
        .next()
        .unwrap()
        .active
        .unwrap();
    assert_eq!(pending.report.fetched, 2);
    assert_eq!(store.inspect("", 10, false).unwrap().len(), 2);
    drop(provider);
    drop(store);

    let (endpoint, handle) = server(vec![
        Reply {
            status: 200,
            body: export,
            expected: "tv_series_ids_01_02_1970.json.gz",
        },
        reply(
            "/tv/2?",
            json!({"id":2,"seasons":[{"id":20,"season_number":1}]}),
        ),
        reply("/tv/2/season/1?", json!({"id":20,"season_number":1})),
        Reply {
            status: 200,
            body: GzEncoder::new(Vec::new(), Compression::default())
                .finish()
                .unwrap(),
            expected: "adult_tv_series_ids_01_02_1970.json.gz",
        },
    ]);
    let mut provider =
        Tmdb::with_endpoints("fixture".into(), &domains, &endpoint, &endpoint).unwrap();
    let mut store = Postgres::connect(database.url.as_str(), "org.themoviedb", &domains).unwrap();
    let summary = sync(&mut provider, &mut store, options, 345600, |_, _| {}).unwrap();
    handle.join().unwrap();
    assert_eq!(summary.id, pending.id);
    assert_eq!(summary.mode, Mode::Full);
    assert_eq!(summary.started_at, 172800);
    assert!(summary.report.complete);
    assert_eq!(summary.report.discovered, 4);
    assert_eq!(summary.report.fetched, 4);
    assert_eq!(summary.report.changed, 4);
    assert_eq!(store.inspect("", 10, false).unwrap().len(), 4);
    let scope = status(&mut store).unwrap().scopes.remove(0);
    assert_eq!(scope.watermark, Some(172800));
    assert!(scope.active.is_none());
}

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
    let mut provider = Tmdb::with_endpoints(
        "fixture".into(),
        &["tv", "season", "episode", "episode_group"],
        &endpoint,
        &endpoint,
    )
    .unwrap();
    let page = provider
        .discover(Mode::Incremental, Some(100000), &Value::Null, 200000)
        .unwrap();
    assert!(page.next.is_none());
    assert_eq!(
        common::fetch(&mut provider, "tv:1").unwrap().unwrap()["id"],
        1
    );
    assert_eq!(
        common::fetch(&mut provider, "season:10").unwrap().unwrap()["id"],
        10
    );
    assert_eq!(
        common::fetch(&mut provider, "episode:100")
            .unwrap()
            .unwrap()["external_ids"]["tvdb_id"],
        12
    );
    assert_eq!(
        common::fetch(&mut provider, "episode_group:group1")
            .unwrap()
            .unwrap()["id"],
        "group1"
    );
    let members: BTreeSet<_> = page
        .changes
        .iter()
        .filter_map(|change| match change {
            Change::Dirty(key) if key != "tv:1" => Some(key.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(members.len(), 3);
    assert!(
        members.contains("season:10")
            && members.contains("episode:100")
            && members.contains("episode_group:group1")
    );
    handle.join().unwrap();
}

#[test]
fn incomplete_hierarchy_never_emits_changes() {
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
    let mut provider = Tmdb::with_endpoints(
        "fixture".into(),
        &["tv", "season", "episode"],
        &endpoint,
        &endpoint,
    )
    .unwrap();
    assert!(
        provider
            .discover(Mode::Incremental, Some(100000), &Value::Null, 200000)
            .is_err()
    );
    handle.join().unwrap();
}

#[test]
fn episode_hierarchy_mismatches_report_address_and_upstream_parentage() {
    for (episode, status) in [
        (
            json!({"id":100,"episode_number":1,"season_number":2,"show_id":1}),
            200,
        ),
        (
            json!({"id":100,"episode_number":1,"season_number":0,"show_id":2}),
            200,
        ),
        (json!({"id":100,"episode_number":1,"season_number":0}), 200),
        (
            json!({"id":100,"episode_number":1,"season_number":0,"show_id":null}),
            200,
        ),
        (
            json!({"id":100,"episode_number":1,"season_number":0,"show_id":2}),
            404,
        ),
    ] {
        let mut replies = vec![
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
                json!({"id":10,"season_number":0,"episodes":[episode]}),
            ),
        ];
        if episode["season_number"] == 0 {
            replies.push(Reply {
                status,
                ..reply(
                    "/tv/1/season/0/episode/1?",
                    json!({"id":101,"episode_number":1,"season_number":0}),
                )
            });
        }
        let (endpoint, handle) = server(replies);
        let mut provider = Tmdb::with_endpoints(
            "fixture".into(),
            &["tv", "season", "episode"],
            &endpoint,
            &endpoint,
        )
        .unwrap();
        let error = provider
            .discover(Mode::Incremental, Some(100000), &Value::Null, 200000)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            format!(
                "TMDB episode 100 at episode:1/0/1 belongs to another season or show (season_number={}, show_id={})",
                episode["season_number"], episode["show_id"]
            )
        );
        handle.join().unwrap();
    }
}

#[test]
fn episode_bootstrap_verifies_unreliable_show_ids_with_native_details() {
    for episode in [
        json!({"id":317139,"episode_number":1,"season_number":12,"show_id":4498}),
        json!({"id":317139,"episode_number":1,"season_number":12}),
        json!({"id":317139,"episode_number":1,"season_number":12,"show_id":null}),
    ] {
        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        writeln!(gzip, "{{\"id\":45}}").unwrap();
        let (endpoint, handle) = server(vec![
            Reply {
                status: 200,
                body: gzip.finish().unwrap(),
                expected: "tv_series_ids_01_02_1970",
            },
            reply(
                "/tv/45?",
                json!({"id":45,"seasons":[{"id":64,"season_number":12}]}),
            ),
            reply(
                "/tv/45/season/12?",
                json!({"id":64,"season_number":12,"episodes":[episode]}),
            ),
            reply(
                "/tv/45/season/12/episode/1?",
                json!({"id":317139,"episode_number":1,"season_number":12,"external_ids":{"tvdb_id":12}}),
            ),
        ]);
        let mut provider = Tmdb::with_endpoints(
            "fixture".into(),
            &["tv", "season", "episode"],
            &endpoint,
            &endpoint,
        )
        .unwrap();
        let page = provider
            .discover(Mode::Full, None, &Value::Null, 172800)
            .unwrap();
        assert_eq!(page.changes.len(), 3);
        assert!(matches!(&page.changes[2], Change::Dirty(key) if key == "episode:317139"));
        assert_eq!(
            provider.descriptor("episode:317139").unwrap(),
            json!({"address":"episode:45/12/1","parent":"tv:45","season_id":64})
        );
        let records = common::fetch_many(&mut provider, &["episode:317139".into()]).unwrap();
        assert_eq!(
            records["episode:317139"].as_ref().unwrap()["external_ids"]["tvdb_id"],
            12
        );
        handle.join().unwrap();
    }
}

#[test]
fn zero_numbered_episodes_are_discovered_and_fetched() {
    for (number, season_path, episode_path) in [
        (0, "/tv/1/season/0?", "/tv/1/season/0/episode/0?"),
        (1, "/tv/1/season/1?", "/tv/1/season/1/episode/0?"),
    ] {
        for show_id in [1, 2] {
            let (endpoint, handle) = server(vec![
                reply(
                    "/tv/changes?page=1",
                    json!({"results":[{"id":1}],"total_pages":1}),
                ),
                reply(
                    "/tv/1?",
                    json!({"id":1,"seasons":[{"id":10,"season_number":number}]}),
                ),
                reply(
                    season_path,
                    json!({"id":10,"season_number":number,"episodes":[{"id":100,"episode_number":0,"season_number":number,"show_id":show_id}]}),
                ),
                reply(
                    episode_path,
                    json!({"id":100,"episode_number":0,"season_number":number,"external_ids":{"tvdb_id":12}}),
                ),
            ]);
            let mut provider =
                Tmdb::with_endpoints("fixture".into(), &["episode"], &endpoint, &endpoint).unwrap();
            let page = provider
                .discover(Mode::Incremental, Some(100000), &Value::Null, 200000)
                .unwrap();
            assert_eq!(page.changes.len(), 1);
            assert!(matches!(&page.changes[0], Change::Dirty(key) if key == "episode:100"));
            assert_eq!(
                provider.descriptor("episode:100").unwrap()["address"],
                format!("episode:1/{number}/0")
            );
            assert_eq!(
                common::fetch(&mut provider, "episode:100")
                    .unwrap()
                    .unwrap()["external_ids"]["tvdb_id"],
                12
            );
            handle.join().unwrap();
        }
    }
}

#[test]
fn invalid_episode_identifiers_report_their_address() {
    for episode_id in [Value::Null, json!(0), json!(-1)] {
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
                json!({"id":10,"season_number":0,"episodes":[{"id":episode_id,"episode_number":0,"season_number":0,"show_id":1}]}),
            ),
        ]);
        let mut provider =
            Tmdb::with_endpoints("fixture".into(), &["episode"], &endpoint, &endpoint).unwrap();
        let error = provider
            .discover(Mode::Incremental, Some(100000), &Value::Null, 200000)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("Invalid TMDB episode ID at episode:1/0/0 (id={episode_id})")
        );
        assert!(format!("{error:#}").contains("Missing positive upstream record ID"));
        handle.join().unwrap();
    }
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
        let mut provider =
            Tmdb::with_endpoints("fixture".into(), &[domain], &endpoint, &endpoint).unwrap();
        let page = provider
            .discover(Mode::Full, None, &Value::Null, 172800)
            .unwrap();
        assert!(matches!(&page.changes[0], Change::Dirty(key) if key == &format!("{domain}:1")));
        let page = provider
            .discover(Mode::Full, None, &page.next.unwrap(), 172800)
            .unwrap();
        assert!(page.next.is_none());
        assert!(
            provider
                .discover(Mode::Incremental, Some(100000), &Value::Null, 200000)
                .unwrap()
                .next
                .is_none()
        );
        handle.join().unwrap();
    }
}

#[test]
fn hierarchy_bootstrap_continues_past_unavailable_exported_shows() {
    for domains in [
        &["tv", "season", "episode", "episode_group"][..],
        &["season", "episode", "episode_group"][..],
    ] {
        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        writeln!(gzip, "{{\"id\":1}}\n{{\"id\":2}}").unwrap();
        let empty = GzEncoder::new(Vec::new(), Compression::default())
            .finish()
            .unwrap();
        let (endpoint, handle) = server(vec![
            Reply {
                status: 200,
                body: gzip.finish().unwrap(),
                expected: "tv_series_ids_01_02_1970",
            },
            Reply {
                status: 404,
                body: b"{}".to_vec(),
                expected: "/tv/1?",
            },
            reply(
                "/tv/2?",
                json!({"id":2,"seasons":[{"id":20,"season_number":1}]}),
            ),
            reply("/tv/2/episode_groups", json!({"results":[]})),
            reply(
                "/tv/2/season/1?",
                json!({"id":20,"season_number":1,"episodes":[{"id":200,"episode_number":1,"season_number":1,"show_id":2}]}),
            ),
            Reply {
                status: 200,
                body: empty,
                expected: "adult_tv_series_ids_01_02_1970",
            },
        ]);
        let mut provider =
            Tmdb::with_endpoints("fixture".into(), domains, &endpoint, &endpoint).unwrap();
        let page = provider
            .discover(Mode::Full, None, &Value::Null, 172800)
            .unwrap();
        assert_eq!(page.next, Some(json!({"export":1,"offset":1})));
        if domains.contains(&"tv") {
            assert_eq!(page.changes.len(), 1);
            assert!(matches!(&page.changes[0], Change::Dirty(key) if key == "tv:1"));
            assert!(common::fetch_many(&mut provider, &["tv:1".into()]).unwrap()["tv:1"].is_none());
        } else {
            assert!(page.changes.is_empty());
        }
        let page = provider
            .discover(Mode::Full, None, &page.next.unwrap(), 172800)
            .unwrap();
        let keys: BTreeSet<_> = page.changes.iter().map(Change::key).collect();
        let mut expected = BTreeSet::from(["season:20", "episode:200"]);
        if domains.contains(&"tv") {
            expected.insert("tv:2");
        }
        assert_eq!(keys, expected);
        assert_eq!(page.next, Some(json!({"export":1,"offset":2})));
        let page = provider
            .discover(Mode::Full, None, &page.next.unwrap(), 172800)
            .unwrap();
        assert!(page.changes.is_empty());
        assert_eq!(page.next, Some(json!({"export":3,"offset":0})));
        let page = provider
            .discover(Mode::Full, None, &page.next.unwrap(), 172800)
            .unwrap();
        assert!(page.changes.is_empty());
        assert!(page.next.is_none());
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
    let mut provider =
        Tmdb::with_endpoints("fixture".into(), &["season"], &endpoint, &endpoint).unwrap();
    let page = provider
        .discover(Mode::Full, None, &Value::Null, 172800)
        .unwrap();
    assert!(matches!(&page.changes[0], Change::Dirty(key) if key == "season:10"));
    assert_eq!(
        common::fetch(&mut provider, "season:10").unwrap().unwrap()["id"],
        10
    );
    let page = provider
        .discover(Mode::Full, None, &page.next.unwrap(), 172800)
        .unwrap();
    let page = provider
        .discover(Mode::Full, None, &page.next.unwrap(), 172800)
        .unwrap();
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
    let mut provider =
        Tmdb::with_endpoints("fixture".into(), &["episode"], &endpoint, &endpoint).unwrap();
    let page = provider
        .discover(Mode::Incremental, Some(100000), &Value::Null, 200000)
        .unwrap();
    assert!(page.next.is_none());
    assert!(matches!(&page.changes[0], Change::Dirty(key) if key == "episode:100"));
    assert_eq!(
        provider.descriptor("episode:100").unwrap()["address"],
        "episode:1/0/1"
    );
    assert_eq!(
        common::fetch(&mut provider, "episode:100")
            .unwrap()
            .unwrap()["external_ids"]["tvdb_id"],
        12
    );
    assert!(common::fetch(&mut provider, "1/../1").is_err());
    handle.join().unwrap();
}

#[test]
fn detail_requests_overlap() {
    use std::{net::TcpListener, thread};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        let first_request = common::read_request(&mut first);
        let (mut second, _) = listener.accept().unwrap();
        let second_request = common::read_request(&mut second);
        for (stream, request) in [(&mut first, first_request), (&mut second, second_request)] {
            let media_id = if request.contains("/movie/1?") { 1 } else { 2 };
            common::write_reply(stream, reply("", json!({"id":media_id})), "0");
        }
    });
    let mut provider =
        Tmdb::with_endpoints("fixture".into(), &["movie", "tv"], &endpoint, &endpoint).unwrap();
    let records = common::fetch_many(&mut provider, &["movie:1".into(), "movie:2".into()]).unwrap();
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
    let mut provider =
        Tmdb::with_endpoints("fixture".into(), &["episode_group"], &endpoint, &endpoint).unwrap();
    let page = provider
        .discover(Mode::Incremental, Some(100000), &Value::Null, 200000)
        .unwrap();
    assert!(matches!(&page.changes[0], Change::Dirty(key) if key == "episode_group:abc123"));
    assert_eq!(page.changes.len(), 1);
    let record = common::fetch(&mut provider, "episode_group:abc123")
        .unwrap()
        .unwrap();
    assert_eq!(record["groups"][0]["episodes"][1]["order"], 0);
    assert_eq!(
        provider.descriptor("episode_group:abc123").unwrap()["parent"],
        "tv:1"
    );
    handle.join().unwrap();
}

#[test]
fn changed_parent_emits_complete_membership_for_database_reconciliation() {
    let (endpoint, handle) = server(vec![
        reply("/tv/changes", json!({"results":[{"id":1}],"total_pages":1})),
        reply(
            "/tv/1?",
            json!({"id":1,"seasons":[{"id":10,"season_number":1}]}),
        ),
        reply(
            "/tv/1/season/1?",
            json!({"id":10,"season_number":1,"episodes":[{"id":100,"show_id":1,"season_number":1,"episode_number":1}]}),
        ),
    ]);
    let mut provider =
        Tmdb::with_endpoints("fixture".into(), &["episode"], &endpoint, &endpoint).unwrap();
    provider.restore(&Value::Null).unwrap();
    let page = provider
        .discover(Mode::Incremental, Some(100000), &Value::Null, 200000)
        .unwrap();
    assert_eq!(page.changes.len(), 1);
    assert!(matches!(&page.changes[0], Change::Dirty(key) if key == "episode:100"));
    let memberships = provider.memberships();
    assert_eq!(memberships.len(), 1);
    assert_eq!(memberships[0].parent, "tv:1");
    assert_eq!(
        memberships[0].members,
        std::collections::BTreeSet::from(["episode:100".into()])
    );
    handle.join().unwrap();
}

#[test]
fn child_addresses_are_explicit_for_each_acquisition() {
    let (endpoint, handle) = server(vec![
        reply(
            "/tv/1/season/1/episode/1?",
            json!({"id":100,"episode_number":1,"season_number":1}),
        ),
        reply(
            "/tv/2/season/2/episode/3?",
            json!({"id":100,"episode_number":3,"season_number":2}),
        ),
    ]);
    let mut provider =
        Tmdb::with_endpoints("fixture".into(), &["episode"], &endpoint, &endpoint).unwrap();
    for address in ["episode:1/1/1", "episode:2/2/3"] {
        let records = provider
            .fetch(&[FetchRequest {
                key: "episode:100".into(),
                descriptor: json!({"address":address}),
            }])
            .unwrap();
        assert_eq!(records[0].as_ref().unwrap()["id"], 100);
        assert!(provider.descriptor("episode:100").is_none());
    }
    handle.join().unwrap();
}

#[test]
fn child_moves_report_both_parent_memberships_in_either_order() {
    for shows in [[1, 2], [2, 1]] {
        let mut replies = vec![reply(
            "/tv/changes",
            json!({"results":shows.map(|show| json!({"id":show})),"total_pages":1}),
        )];
        for show in shows {
            if show == 1 {
                replies.push(reply("/tv/1?", json!({"id":1,"seasons":[]})));
            } else {
                replies.push(reply(
                    "/tv/2?",
                    json!({"id":2,"seasons":[{"id":20,"season_number":1}]}),
                ));
                replies.push(reply("/tv/2/season/1?", json!({"id":20,"season_number":1,"episodes":[{"id":100,"show_id":2,"season_number":1,"episode_number":1}]})));
            }
        }
        let (endpoint, handle) = server(replies);
        let mut provider =
            Tmdb::with_endpoints("fixture".into(), &["episode"], &endpoint, &endpoint).unwrap();
        provider.restore(&Value::Null).unwrap();
        let page = provider
            .discover(Mode::Incremental, Some(100000), &Value::Null, 200000)
            .unwrap();
        assert_eq!(page.changes.len(), 1);
        assert!(matches!(&page.changes[0], Change::Dirty(key) if key == "episode:100"));
        assert_eq!(
            provider.descriptor("episode:100").unwrap()["parent"],
            "tv:2"
        );
        assert_eq!(provider.memberships().len(), 2);
        handle.join().unwrap();
    }
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
    let mut provider =
        Tmdb::with_endpoints("fixture".into(), &["episode"], &endpoint, &endpoint).unwrap();
    provider.restore(&Value::Null).unwrap();
    let page = provider
        .discover(Mode::Incremental, Some(100000), &Value::Null, 200000)
        .unwrap();
    assert!(matches!(&page.changes[0], Change::Dirty(key) if key == "episode:100"));
    assert_eq!(
        provider.descriptor("episode:100").unwrap()["address"],
        "episode:1/2/1"
    );
    assert_eq!(
        common::fetch(&mut provider, "episode:100")
            .unwrap()
            .unwrap()["id"],
        100
    );
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
    let mut provider =
        Tmdb::with_endpoints("fixture".into(), &["movie", "tv"], &endpoint, &endpoint).unwrap();
    assert_eq!(
        common::fetch_many(&mut provider, &["movie:1".into(), "movie:2".into()])
            .unwrap()
            .len(),
        2
    );
    handle.join().unwrap();
}

#[test]
fn interrupted_detail_bodies_are_retried_before_returning_records() {
    use std::{net::TcpListener, thread};

    for (key, path, record) in [
        (
            "episode:8590/8/17",
            "/tv/8590/season/8/episode/17?append_to_response=external_ids",
            json!({"id":123,"season_number":8,"episode_number":17}),
        ),
        (
            "episode_group:group1",
            "/tv/episode_group/group1",
            json!({"id":"group1","groups":[]}),
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let response = record.clone();
        let handle = thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = common::read_request(&mut stream);
                assert!(request.contains(path), "{request}");
                if attempt == 0 {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{{"
                    )
                    .unwrap();
                } else {
                    common::write_reply(&mut stream, reply(path, response.clone()), "0");
                }
            }
        });
        let mut provider = Tmdb::with_endpoints(
            "fixture".into(),
            &["episode", "episode_group"],
            &endpoint,
            &endpoint,
        )
        .unwrap();
        let records = common::fetch_many(&mut provider, &[key.into()]).unwrap();
        assert_eq!(records[key].as_ref(), Some(&record));
        handle.join().unwrap();
    }
}

#[test]
fn failed_batch_never_returns_partial_records() {
    let (endpoint, handle) = server(vec![Reply {
        status: 401,
        body: vec![],
        expected: "/movie/1",
    }]);
    let mut provider =
        Tmdb::with_endpoints("fixture".into(), &["movie", "tv"], &endpoint, &endpoint).unwrap();
    assert!(common::fetch_many(&mut provider, &["movie:1".into()]).is_err());
    handle.join().unwrap();
    let (endpoint, handle) = server(vec![reply("/movie/1", json!({"id":999}))]);
    let mut provider =
        Tmdb::with_endpoints("fixture".into(), &["movie", "tv"], &endpoint, &endpoint).unwrap();
    assert!(common::fetch_many(&mut provider, &["movie:1".into()]).is_err());
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
    let mut provider =
        Tmdb::with_endpoints("fixture".into(), &["movie", "tv"], &endpoint, &endpoint).unwrap();
    let first = provider
        .discover(Mode::Incremental, Some(172800), &Value::Null, 259200)
        .unwrap();
    assert!(matches!(&first.changes[0],Change::Dirty(key) if key == "movie:1"));
    let second = provider
        .discover(
            Mode::Incremental,
            Some(172800),
            &first.next.unwrap(),
            259200,
        )
        .unwrap();
    let third = provider
        .discover(
            Mode::Incremental,
            Some(172800),
            &second.next.unwrap(),
            259200,
        )
        .unwrap();
    assert!(matches!(&third.changes[0],Change::Dirty(key) if key == "tv:1"));
    assert!(third.next.is_none());
    assert_eq!(
        common::fetch(&mut provider, "movie:1").unwrap().unwrap()["native_field"],
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
    let mut provider =
        Tmdb::with_endpoints("fixture".into(), &["movie", "tv"], &endpoint, &endpoint).unwrap();
    let mut cursor = Value::Null;
    let mut keys = Vec::new();
    loop {
        let page = provider
            .discover(Mode::Full, None, &cursor, 172800)
            .unwrap();
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
    let mut provider =
        Tmdb::with_endpoints("fixture".into(), &["movie", "tv"], &endpoint, &endpoint).unwrap();
    assert!(common::fetch(&mut provider, "movie:1").unwrap().is_some());
    assert!(common::fetch(&mut provider, "movie:2").is_err());
    assert!(common::fetch(&mut provider, "movie:3").unwrap().is_none());
    handle.join().unwrap();
}
