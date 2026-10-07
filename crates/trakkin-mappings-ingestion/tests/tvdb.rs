mod common;

use common::{Reply, reply, server};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use trakkin_mappings_ingestion::{Change, Operation, Provider, providers::Tvdb, storage::Entry};

#[test]
fn multi_domain_catalogues_skip_unselected_endpoints() {
    let (endpoint, handle) = server(vec![
        reply("/login", json!({"data":{"token":"fixture"}})),
        reply(
            "/movies?page=0",
            json!({"status":"success","data":[{"id":1}],"links":{"next":null}}),
        ),
        reply(
            "/episodes?page=0",
            json!({"status":"success","data":[{"id":2}],"links":{"next":null}}),
        ),
    ]);
    let mut provider =
        Tvdb::with_endpoint("fixture".into(), None, &["movie", "episode"], &endpoint).unwrap();
    let movies = provider
        .discover(Operation::Bootstrap, None, &Value::Null, 100)
        .unwrap();
    let episodes = provider
        .discover(Operation::Bootstrap, None, &movies.next.unwrap(), 100)
        .unwrap();
    assert_eq!(movies.changes[0].key(), "movie:1");
    assert_eq!(episodes.changes[0].key(), "episode:2");
    assert!(episodes.next.is_none());
    handle.join().unwrap();
}

#[test]
fn reconciliation_confirms_absences_only_after_selected_catalogue_completion() {
    let (endpoint, handle) = server(vec![
        reply("/login", json!({"data":{"token":"fixture"}})),
        reply(
            "/movies?page=0",
            json!({"status":"success","data":[{"id":1}],"links":{"next":"?page=1"}}),
        ),
        reply(
            "/movies?page=1",
            json!({"status":"success","data":[],"links":{"next":null}}),
        ),
        Reply {
            status: 404,
            body: b"{}".to_vec(),
            expected: "/movies/2/extended",
        },
    ]);
    let mut provider = Tvdb::with_endpoint("fixture".into(), None, &["movie"], &endpoint).unwrap();
    let index = BTreeMap::from([
        (
            "movie:1".into(),
            Entry {
                hash: "old".into(),
                deleted: false,
                metadata: json!({}),
            },
        ),
        (
            "movie:2".into(),
            Entry {
                hash: "old".into(),
                deleted: false,
                metadata: json!({}),
            },
        ),
        (
            "series:3".into(),
            Entry {
                hash: "old".into(),
                deleted: false,
                metadata: json!({}),
            },
        ),
        (
            "movie:4".into(),
            Entry {
                hash: "old".into(),
                deleted: true,
                metadata: json!({}),
            },
        ),
    ]);
    provider
        .restore(Operation::Reconcile, &index, &Value::Null)
        .unwrap();
    let first = provider
        .discover(Operation::Reconcile, None, &Value::Null, 100)
        .unwrap();
    assert!(matches!(&first.changes[..], [Change::Dirty(key)] if key == "movie:1"));
    assert_eq!(first.next, Some(json!({"kind":0,"page":1})));
    let last = provider
        .discover(Operation::Reconcile, None, &first.next.unwrap(), 100)
        .unwrap();
    assert!(last.changes.is_empty());
    assert_eq!(last.next, Some(json!({"absent":0})));
    let absent = provider
        .discover(Operation::Reconcile, None, &last.next.unwrap(), 100)
        .unwrap();
    assert!(matches!(&absent.changes[..], [Change::Dirty(key)] if key == "movie:2"));
    assert!(absent.next.is_none());
    assert!(provider.fetch("movie:2").unwrap().is_none());
    handle.join().unwrap();
}

#[test]
fn reconciliation_rejects_empty_selected_catalogues_before_absence_checks() {
    let (endpoint, handle) = server(vec![
        reply("/login", json!({"data":{"token":"fixture"}})),
        reply(
            "/episodes?page=0",
            json!({"status":"success","data":[],"links":{"next":null}}),
        ),
    ]);
    let mut provider =
        Tvdb::with_endpoint("fixture".into(), None, &["episode"], &endpoint).unwrap();
    provider
        .restore(Operation::Reconcile, &BTreeMap::new(), &Value::Null)
        .unwrap();
    assert!(
        provider
            .discover(Operation::Reconcile, None, &Value::Null, 100)
            .is_err()
    );
    handle.join().unwrap();
}

#[test]
fn series_snapshot_preserves_parents_and_unaffected_paginated_orders() {
    use std::{net::TcpListener, thread};
    let episode = |number, absolute| json!({"id":12,"seriesId":1,"seasonNumber":1,"number":number,"absoluteNumber":absolute});
    let series = json!({"id":1,"name":"Fixture series","defaultSeasonType":1,"seasonTypes":[{"id":1,"type":"official"},{"id":3,"type":"absolute"}],"seasons":[{"id":20,"seriesId":1,"number":1}]});
    let expected_series = series.clone();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        for response in [
            json!({"data":{"token":"fixture"}}),
            json!({"status":"success","data":series}),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            common::read_request(&mut stream);
            common::write_reply(&mut stream, reply("", response), "0");
        }
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = common::read_request(&mut stream);
            let absolute = request.contains("/absolute?");
            let next = if absolute {
                Value::Null
            } else {
                json!("?page=1")
            };
            let episodes = if absolute {
                json!([episode(7,7),episode(8,7),{"id":14,"seriesId":1,"seasonNumber":1,"number":9,"absoluteNumber":9}])
            } else {
                json!([episode(2, 7)])
            };
            let payload = json!({"status":"success","data":{"series":{"id":1},"episodes":episodes},"links":{"next":next}});
            common::write_reply(&mut stream, reply("", payload), "0");
        }
        let (mut stream, _) = listener.accept().unwrap();
        assert!(common::read_request(&mut stream).contains("/official?page=1"));
        common::write_reply(
            &mut stream,
            reply(
                "",
                json!({"status":"success","data":{"series":{"id":1},"episodes":[{"id":13,"seriesId":1,"seasonNumber":0,"number":1}]},"links":{"next":null}}),
            ),
            "0",
        );
    });
    let mut provider =
        Tvdb::with_endpoint("fixture".into(), None, Tvdb::DOMAINS, &endpoint).unwrap();
    let snapshot = provider.fetch("series:1").unwrap().unwrap();
    assert_eq!(snapshot["series"], expected_series);
    assert_eq!(snapshot["episode_orders"]["official"][0]["number"], 2);
    assert_eq!(
        snapshot["episode_orders"]["absolute"],
        json!([{"id":14,"seriesId":1,"seasonNumber":1,"number":9,"absoluteNumber":9}])
    );
    assert_eq!(snapshot["episode_orders"]["official"][1]["seasonNumber"], 0);
    handle.join().unwrap();
}

#[test]
fn conflicting_ordered_episodes_are_skipped_without_losing_parents_or_orders() {
    for (order, case, same_page, reversed) in [
        ("absolute", "number", false, false),
        ("absolute", "number", false, true),
        ("absolute", "number", true, false),
        ("absolute", "number", true, true),
        ("absolute", "metadata", true, false),
        ("absolute", "identical", true, false),
        ("absolute", "all_conflicting", true, false),
        ("official", "number", false, true),
        ("official", "identical", false, false),
        ("dvd", "number", true, false),
    ] {
        let episode =
            json!({"id":14703,"seriesId":70600,"seasonNumber":1,"number":2,"absoluteNumber":63});
        let mut changed_episode = episode.clone();
        changed_episode["number"] = json!(63);
        match case {
            "metadata" => changed_episode["name"] = json!("Different title"),
            "identical" => changed_episode = episode.clone(),
            _ => {}
        }
        let unique_episode =
            json!({"id":5640498,"seriesId":70600,"seasonNumber":1,"number":1,"absoluteNumber":1});
        let (first, second) = if reversed {
            (changed_episode.clone(), episode.clone())
        } else {
            (episode.clone(), changed_episode.clone())
        };
        let mut first_page = if same_page {
            json!([unique_episode.clone(), first.clone(), second.clone()])
        } else {
            json!([unique_episode.clone(), first.clone()])
        };
        let mut final_page = json!([
            second.clone(),
            first.clone(),
            second.clone(),
            unique_episode.clone()
        ]);
        if case == "all_conflicting" {
            first_page = json!([first.clone(), second.clone()]);
            final_page = json!([second.clone(), first.clone()]);
        }
        let series = json!({"id":70600,"name":"Dateline NBC","defaultSeasonType":1,"seasonTypes":[{"id":1,"type":order}],"seasons":[{"id":1736331,"seriesId":70600,"number":1}]});
        let (endpoint, handle) = server(vec![
            reply("/login", json!({"data":{"token":"fixture"}})),
            reply(
                "/series/70600/extended",
                json!({"status":"success","data":series.clone()}),
            ),
            reply(
                match order {
                    "absolute" => "/series/70600/episodes/absolute?page=0",
                    "official" => "/series/70600/episodes/official?page=0",
                    _ => "/series/70600/episodes/dvd?page=0",
                },
                json!({"status":"success","data":{"series":{"id":70600},"episodes":first_page},"links":{"next":"?page=1"}}),
            ),
            reply(
                match order {
                    "absolute" => "/series/70600/episodes/absolute?page=1",
                    "official" => "/series/70600/episodes/official?page=1",
                    _ => "/series/70600/episodes/dvd?page=1",
                },
                json!({"status":"success","data":{"series":{"id":70600},"episodes":final_page},"links":{"next":null}}),
            ),
        ]);
        let mut provider =
            Tvdb::with_endpoint("fixture".into(), None, Tvdb::DOMAINS, &endpoint).unwrap();
        let snapshot = provider.fetch("series:70600").unwrap().unwrap();
        assert_eq!(snapshot["series"], series);
        let expected = match case {
            "identical" => json!([unique_episode, episode]),
            "all_conflicting" => json!([]),
            _ => json!([unique_episode]),
        };
        assert_eq!(
            snapshot["episode_orders"][order], expected,
            "order={order}, case={case}, same_page={same_page}, reversed={reversed}"
        );
        handle.join().unwrap();
    }
}

#[test]
fn incomplete_order_pages_fail_the_series_acquisition() {
    let (endpoint, handle) = server(vec![
        reply("/login", json!({"data":{"token":"fixture"}})),
        reply(
            "/series/1/extended",
            json!({"status":"success","data":{"id":1,"defaultSeasonType":1,"seasonTypes":[{"id":1,"type":"official"}]}}),
        ),
        reply(
            "page=0",
            json!({"status":"success","data":{"series":{"id":1},"episodes":[{"id":12,"seriesId":1,"seasonNumber":1,"number":1}]},"links":{"next":"?page=1"}}),
        ),
        reply(
            "page=1",
            json!({"status":"success","data":{"series":{"id":1},"episodes":[]}}),
        ),
    ]);
    let mut provider =
        Tvdb::with_endpoint("fixture".into(), None, Tvdb::DOMAINS, &endpoint).unwrap();
    assert_eq!(
        provider.fetch("series:1").unwrap_err().to_string(),
        "Missing TVDB pagination links"
    );
    handle.join().unwrap();
}

#[test]
fn series_sync_refreshes_order_snapshots_for_child_updates() {
    let (endpoint, handle) = server(vec![
        reply("/login", json!({"data":{"token":"fixture"}})),
        reply(
            "/updates?since=13600&page=0",
            json!({"status":"success","data":[
            {"recordType":"episodes","recordId":12,"seriesId":1,"methodInt":3},
            {"recordType":"seasons","recordId":20,"methodInt":2}
        ],"links":{"next":null}}),
        ),
        reply(
            "/seasons/20/extended",
            json!({"status":"success","data":{"id":20,"seriesId":2}}),
        ),
    ]);
    let mut provider = Tvdb::with_endpoint("fixture".into(), None, &["series"], &endpoint).unwrap();
    let page = provider
        .discover(Operation::Sync, Some(100000), &Value::Null, 200000)
        .unwrap();
    assert!(matches!(&page.changes[0], Change::Dirty(key) if key == "series:1"));
    assert!(matches!(&page.changes[1], Change::Dirty(key) if key == "series:2"));
    handle.join().unwrap();
}

#[test]
fn season_type_changes_refresh_all_series_then_resume_updates() {
    let (endpoint, handle) = server(vec![
        reply("/login", json!({"data":{"token":"fixture"}})),
        reply(
            "/updates?since=13600&page=0",
            json!({"status":"success","data":[{"recordType":"seasontypes","recordId":1,"methodInt":2}],"links":{"next":"?page=1"}}),
        ),
        reply(
            "/series?page=0",
            json!({"status":"success","data":[{"id":1}],"links":{"next":"?page=1"}}),
        ),
        reply(
            "/series?page=1",
            json!({"status":"success","data":[{"id":2}],"links":{"next":null}}),
        ),
        reply(
            "/updates?since=13600&page=1",
            json!({"status":"success","data":[{"recordType":"series","recordId":3,"methodInt":3}],"links":{"next":null}}),
        ),
    ]);
    let mut provider = Tvdb::with_endpoint("fixture".into(), None, &["series"], &endpoint).unwrap();
    let first = provider
        .discover(Operation::Sync, Some(100000), &Value::Null, 200000)
        .unwrap();
    assert!(matches!(&first.changes[0], Change::Dirty(key) if key == "series:1"));
    let second = provider
        .discover(Operation::Sync, Some(100000), &first.next.unwrap(), 200000)
        .unwrap();
    assert!(matches!(&second.changes[0], Change::Dirty(key) if key == "series:2"));
    let last = provider
        .discover(Operation::Sync, Some(100000), &second.next.unwrap(), 200000)
        .unwrap();
    assert!(matches!(&last.changes[0], Change::Deleted {key, ..} if key == "series:3"));
    assert!(last.next.is_none());
    handle.join().unwrap();
}

#[test]
fn multi_domain_updates_refresh_series_without_losing_child_events() {
    let (endpoint, handle) = server(vec![
        reply("/login", json!({"data":{"token":"fixture"}})),
        reply(
            "/updates?since=13600&page=0",
            json!({"status":"success","data":[
            {"recordType":"episodes","recordId":12,"seriesId":1,"methodInt":3},
            {"recordType":"seasons","recordId":20,"methodInt":2},
            {"recordType":"seasons","recordId":20,"methodInt":2}
        ],"links":{"next":null}}),
        ),
        reply(
            "/seasons/20/extended",
            json!({"status":"success","data":{"id":20,"seriesId":2}}),
        ),
    ]);
    let mut provider = Tvdb::with_endpoint(
        "fixture".into(),
        None,
        &["series", "season", "episode"],
        &endpoint,
    )
    .unwrap();
    let page = provider
        .discover(Operation::Sync, Some(100000), &Value::Null, 200000)
        .unwrap();
    assert!(
        page.changes
            .iter()
            .any(|change| matches!(change, Change::Deleted {key, ..} if key == "episode:12"))
    );
    assert_eq!(
        page.changes
            .iter()
            .filter(|change| matches!(change, Change::Dirty(key) if key == "season:20"))
            .count(),
        2
    );
    assert!(
        page.changes
            .iter()
            .any(|change| matches!(change, Change::Dirty(key) if key == "series:2"))
    );
    handle.join().unwrap();
}

#[test]
fn child_catalogues_fetch_extended_records_and_preserve_deletions() {
    for (domain, path) in [("season", "seasons"), ("episode", "episodes")] {
        let (endpoint, handle) = server(vec![
            reply("/login", json!({"data":{"token":"fixture"}})),
            reply(
                "page=0",
                json!({"status":"success","data":[{"id":12}],"links":{"next":"?page=1"}}),
            ),
            reply(
                "page=1",
                json!({"status":"success","data":[],"links":{"next":null}}),
            ),
            reply(
                "/12/extended",
                json!({"status":"success","data":{"id":12,"seriesId":1,"number":0}}),
            ),
            reply(
                if domain == "season" {
                    "type=seasons"
                } else {
                    "type=episodes"
                },
                json!({"status":"success","data":[{"recordType":path,"recordId":12,"methodInt":3}],"links":{"next":null}}),
            ),
        ]);
        let mut provider =
            Tvdb::with_endpoint("fixture".into(), None, &[domain], &endpoint).unwrap();
        let page = provider
            .discover(Operation::Bootstrap, None, &Value::Null, 0)
            .unwrap();
        assert!(matches!(&page.changes[0], Change::Dirty(key) if key == &format!("{domain}:12")));
        assert!(
            provider
                .discover(Operation::Bootstrap, None, &page.next.unwrap(), 0)
                .unwrap()
                .next
                .is_none()
        );
        assert_eq!(
            provider.fetch(&format!("{domain}:12")).unwrap().unwrap()["seriesId"],
            1
        );
        let page = provider
            .discover(Operation::Sync, Some(100000), &Value::Null, 200000)
            .unwrap();
        assert!(
            matches!(&page.changes[0], Change::Deleted {key, ..} if key == &format!("{domain}:12"))
        );
        handle.join().unwrap();
    }
}

#[test]
fn detail_batches_overlap_under_the_shared_request_limiter() {
    use std::{
        net::TcpListener,
        thread,
        time::{Duration, Instant},
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let (mut login, _) = listener.accept().unwrap();
        common::read_request(&mut login);
        common::write_reply(
            &mut login,
            reply("", json!({"data":{"token":"fixture"}})),
            "0",
        );
        let (mut first, _) = listener.accept().unwrap();
        let first_request = common::read_request(&mut first);
        let (mut second, _) = listener.accept().unwrap();
        let second_request = common::read_request(&mut second);
        for (stream, request) in [(&mut first, first_request), (&mut second, second_request)] {
            let record_id = if request.contains("/episodes/1/") {
                1
            } else {
                2
            };
            common::write_reply(
                stream,
                reply("", json!({"status":"success","data":{"id":record_id}})),
                "0",
            );
        }
    });
    let mut provider =
        Tvdb::with_endpoint("fixture".into(), None, Tvdb::DOMAINS, &endpoint).unwrap();
    let started = Instant::now();
    assert_eq!(
        provider
            .fetch_many(&["episode:1".into(), "episode:2".into()])
            .unwrap()
            .len(),
        2
    );
    assert!(started.elapsed() >= Duration::from_millis(20));
    handle.join().unwrap();
}

#[test]
fn preserves_merges_and_follows_next_page_without_following_its_host() {
    let (endpoint, handle) = server(vec![
        reply("/login", json!({"data":{"token":"fixture"}})),
        reply(
            "/updates?since=113600&page=0",
            json!({"status":"success","data":[{"recordType":"series","recordId":1,"methodInt":3,"mergeToId":2,"mergeToEntityType":"series"}],"links":{"next":"https://untrusted.example/updates?page=1"}}),
        ),
        reply(
            "/updates?since=113600&page=1",
            json!({"status":"success","data":[],"links":{"next":null}}),
        ),
    ]);
    let mut provider =
        Tvdb::with_endpoint("fixture".into(), None, Tvdb::DOMAINS, &endpoint).unwrap();
    let page = provider
        .discover(Operation::Sync, Some(200000), &Value::Null, 300000)
        .unwrap();
    assert!(
        matches!(&page.changes[0],Change::Deleted { key, metadata } if key == "series:1" && metadata["mergeToId"] == 2)
    );
    assert!(matches!(&page.changes[1],Change::Dirty(key) if key == "series:2"));
    let end = provider
        .discover(Operation::Sync, Some(200000), &page.next.unwrap(), 300000)
        .unwrap();
    assert!(end.next.is_none());
    handle.join().unwrap();
}
