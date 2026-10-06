mod common;

use common::{reply, server};
use serde_json::{Value, json};
use trakkin_mappings_ingestion::{Change, Provider, providers::Tvdb};

#[test]
fn series_snapshot_preserves_independent_paginated_orders() {
    use std::{net::TcpListener, thread};
    let episode = |number, absolute| json!({"id":12,"seriesId":1,"seasonNumber":1,"number":number,"absoluteNumber":absolute});
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        for response in [
            json!({"data":{"token":"fixture"}}),
            json!({"status":"success","data":{"id":1,"defaultSeasonType":1,"seasonTypes":[{"id":1,"type":"official"},{"id":2,"type":"absolute"}]}}),
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
            let payload = json!({"status":"success","data":{"series":{"id":1},"episodes":[episode(if absolute {7} else {2},7)]},"links":{"next":next}});
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
    let mut provider = Tvdb::with_endpoint("fixture".into(), None, &endpoint).unwrap();
    let snapshot = provider.fetch("series:1").unwrap().unwrap();
    assert_eq!(snapshot["series"]["defaultSeasonType"], 1);
    assert_eq!(snapshot["episode_orders"]["official"][0]["number"], 2);
    assert_eq!(snapshot["episode_orders"]["absolute"][0]["number"], 7);
    assert_eq!(snapshot["episode_orders"]["official"][1]["seasonNumber"], 0);
    handle.join().unwrap();
}

#[test]
fn incomplete_or_duplicate_order_pages_fail_the_series_acquisition() {
    for duplicate in [false, true] {
        let episode = json!({"id":12,"seriesId":1,"seasonNumber":1,"number":1});
        let final_page = if duplicate {
            json!({"status":"success","data":{"series":{"id":1},"episodes":[episode.clone()]},"links":{"next":null}})
        } else {
            json!({"status":"success","data":{"series":{"id":1},"episodes":[]}})
        };
        let (endpoint, handle) = server(vec![
            reply("/login", json!({"data":{"token":"fixture"}})),
            reply(
                "/series/1/extended",
                json!({"status":"success","data":{"id":1,"defaultSeasonType":1,"seasonTypes":[{"id":1,"type":"official"}]}}),
            ),
            reply(
                "page=0",
                json!({"status":"success","data":{"series":{"id":1},"episodes":[episode]},"links":{"next":"?page=1"}}),
            ),
            reply("page=1", final_page),
        ]);
        let mut provider = Tvdb::with_endpoint("fixture".into(), None, &endpoint).unwrap();
        assert!(provider.fetch("series:1").is_err());
        handle.join().unwrap();
    }
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
    let mut provider = Tvdb::with_endpoint("fixture".into(), None, &endpoint).unwrap();
    provider.select_domain("series");
    let page = provider
        .discover_changes(100000, &Value::Null, 200000)
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
    let mut provider = Tvdb::with_endpoint("fixture".into(), None, &endpoint).unwrap();
    provider.select_domain("series");
    let first = provider
        .discover_changes(100000, &Value::Null, 200000)
        .unwrap();
    assert!(matches!(&first.changes[0], Change::Dirty(key) if key == "series:1"));
    let second = provider
        .discover_changes(100000, &first.next.unwrap(), 200000)
        .unwrap();
    assert!(matches!(&second.changes[0], Change::Dirty(key) if key == "series:2"));
    let last = provider
        .discover_changes(100000, &second.next.unwrap(), 200000)
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
    let provider = Tvdb::with_endpoint("fixture".into(), None, &endpoint).unwrap();
    let mut plan = trakkin_mappings_ingestion::dataset::AcquisitionPlan::new(
        Box::new(provider),
        &["series", "season", "episode"],
    )
    .unwrap();
    let page = plan.discover_changes(100000, &Value::Null, 200000).unwrap();
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
        let mut provider = Tvdb::with_endpoint("fixture".into(), None, &endpoint).unwrap();
        provider.select_domain(domain);
        let page = provider.enumerate(&Value::Null, 0).unwrap();
        assert!(matches!(&page.changes[0], Change::Dirty(key) if key == &format!("{domain}:12")));
        assert!(
            provider
                .enumerate(&page.next.unwrap(), 0)
                .unwrap()
                .next
                .is_none()
        );
        assert_eq!(
            provider.fetch(&format!("{domain}:12")).unwrap().unwrap()["seriesId"],
            1
        );
        let page = provider
            .discover_changes(100000, &Value::Null, 200000)
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
        let started = Instant::now();
        let (mut second, _) = listener.accept().unwrap();
        let second_request = common::read_request(&mut second);
        assert!(started.elapsed() >= Duration::from_millis(20));
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
    let mut provider = Tvdb::with_endpoint("fixture".into(), None, &endpoint).unwrap();
    assert_eq!(
        provider
            .fetch_many(&["episode:1".into(), "episode:2".into()])
            .unwrap()
            .len(),
        2
    );
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
    let mut provider = Tvdb::with_endpoint("fixture".into(), None, &endpoint).unwrap();
    let page = provider
        .discover_changes(200000, &Value::Null, 300000)
        .unwrap();
    assert!(
        matches!(&page.changes[0],Change::Deleted { key, metadata } if key == "series:1" && metadata["mergeToId"] == 2)
    );
    assert!(matches!(&page.changes[1],Change::Dirty(key) if key == "series:2"));
    let end = provider
        .discover_changes(200000, &page.next.unwrap(), 300000)
        .unwrap();
    assert!(end.next.is_none());
    handle.join().unwrap();
}
