mod common;

use common::{Reply, reply, server};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use trakkin_mappings_ingestion::{
    Change, Operation, Page, Provider, providers::AniList, storage::Entry,
};

fn fixture_provider(endpoint: &str) -> AniList {
    AniList::with_endpoint_and_interval(endpoint, Duration::ZERO).unwrap()
}

fn media_response(first: u64, count: usize, records: &[Value], latest: Option<u64>) -> Value {
    let mut data = serde_json::Map::new();
    if let Some(latest) = latest {
        data.insert(
            "latest".into(),
            if latest == 0 {
                Value::Null
            } else {
                json!({"id":latest})
            },
        );
    }
    for offset in 0..count.div_ceil(50) {
        let start = first + (offset * 50) as u64;
        let end = (start + 50).min(first + count as u64);
        let media: Vec<_> = records
            .iter()
            .filter(|record| {
                let media_id = record["id"].as_u64().unwrap();
                media_id >= start && media_id < end
            })
            .cloned()
            .collect();
        data.insert(format!("batch{offset}"), json!({"media":media}));
    }
    json!({"data":data})
}

fn airing_response(pages: Vec<(bool, Vec<Value>)>) -> Value {
    let data: serde_json::Map<_, _> = pages
        .into_iter()
        .enumerate()
        .map(|(offset, (has_next, schedules))| {
            (
                format!("page{offset}"),
                json!({"pageInfo":{"hasNextPage":has_next},"airingSchedules":schedules}),
            )
        })
        .collect();
    json!({"data":data})
}

fn dirty_keys(page: &Page) -> Vec<&str> {
    page.changes
        .iter()
        .map(|change| match change {
            Change::Dirty(key) => key.as_str(),
            _ => panic!("Expected dirty change"),
        })
        .collect()
}

#[test]
fn sync_discovers_new_ids_and_recent_airings_without_reading_old_records() {
    let (endpoint, handle) = server(vec![
        reply(
            "\"ids0\":[11,12",
            media_response(11, 8000, &[json!({"id":12})], Some(12)),
        ),
        reply(
            "\"after\":790399",
            airing_response(vec![(
                false,
                vec![
                    json!({"mediaId":1,"airingAt":790400}),
                    json!({"mediaId":12,"airingAt":2000010}),
                    json!({"mediaId":1,"airingAt":2000000}),
                ],
            )]),
        ),
    ]);
    let mut provider = fixture_provider(&endpoint);
    provider
        .restore(Operation::Sync, &BTreeMap::new(), &json!({"after":10}))
        .unwrap();
    let page = provider
        .discover(Operation::Sync, Some(2000000), &Value::Null, 2000010)
        .unwrap();
    assert_eq!(dirty_keys(&page), vec!["media:1", "media:12"]);
    assert!(page.next.is_none());
    assert_eq!(provider.checkpoint(), json!({"after":12}));
    handle.join().unwrap();
}

#[test]
fn airing_refresh_does_not_advance_new_id_discovery() {
    let (endpoint, handle) = server(vec![
        reply("\"ids0\":[11,12", media_response(11, 8000, &[], Some(10))),
        reply(
            "airingSchedules",
            airing_response(vec![(
                false,
                vec![json!({"mediaId":100,"airingAt":2000000})],
            )]),
        ),
        reply(
            "\"ids0\":[11,12",
            media_response(11, 8000, &[json!({"id":11}), json!({"id":12})], Some(12)),
        ),
        reply("airingSchedules", airing_response(vec![(false, vec![])])),
    ]);
    let mut provider = fixture_provider(&endpoint);
    provider
        .restore(Operation::Sync, &BTreeMap::new(), &json!({"after":10}))
        .unwrap();
    let first = provider
        .discover(Operation::Sync, Some(2000000), &Value::Null, 2000010)
        .unwrap();
    assert_eq!(dirty_keys(&first), vec!["media:100"]);
    assert_eq!(provider.checkpoint(), json!({"after":10}));
    let second = provider
        .discover(Operation::Sync, Some(2000010), &Value::Null, 2000020)
        .unwrap();
    assert_eq!(dirty_keys(&second), vec!["media:11", "media:12"]);
    assert_eq!(provider.checkpoint(), json!({"after":12}));
    handle.join().unwrap();
}

#[test]
fn sync_covers_long_gaps_since_previous_success() {
    let (endpoint, handle) = server(vec![
        reply("latest:Media", media_response(1, 8000, &[], Some(0))),
        reply(
            "\"after\":790399",
            airing_response(vec![(false, vec![json!({"mediaId":1,"airingAt":2000001})])]),
        ),
    ]);
    let mut provider = fixture_provider(&endpoint);
    let page = provider
        .discover(Operation::Sync, Some(2000000), &Value::Null, 4000000)
        .unwrap();
    assert_eq!(dirty_keys(&page), vec!["media:1"]);
    assert_eq!(provider.checkpoint(), json!({"after":0}));
    handle.join().unwrap();
}

#[test]
fn empty_checkpoint_starts_id_discovery_at_zero() {
    let (endpoint, handle) = server(vec![
        reply(
            "latest:Media",
            media_response(1, 8000, &[json!({"id":1})], Some(1)),
        ),
        reply("airingSchedules", airing_response(vec![(false, vec![])])),
    ]);
    let mut provider = fixture_provider(&endpoint);
    provider
        .restore(Operation::Sync, &BTreeMap::new(), &Value::Null)
        .unwrap();
    let page = provider
        .discover(Operation::Sync, Some(100), &Value::Null, 200)
        .unwrap();
    assert_eq!(dirty_keys(&page), vec!["media:1"]);
    assert_eq!(provider.checkpoint(), json!({"after":1}));
    handle.join().unwrap();
}

#[test]
fn airing_pages_deduplicate_ids_and_follow_successors() {
    let (endpoint, handle) = server(vec![
        reply("latest:Media", media_response(1, 8000, &[], Some(0))),
        reply(
            "page82:Page(page:$page82",
            airing_response(vec![
                (true, vec![json!({"mediaId":1,"airingAt":100})]),
                (
                    false,
                    vec![
                        json!({"mediaId":1,"airingAt":100}),
                        json!({"mediaId":2,"airingAt":200}),
                    ],
                ),
            ]),
        ),
    ]);
    let mut provider = fixture_provider(&endpoint);
    let page = provider
        .discover(Operation::Sync, Some(100), &Value::Null, 200)
        .unwrap();
    assert_eq!(dirty_keys(&page), vec!["media:1", "media:2"]);
    assert!(page.next.is_none());
    handle.join().unwrap();
}

#[test]
fn airing_windows_split_before_exceeding_pagination_depth() {
    let schedule = json!({"mediaId":1,"airingAt":5});
    let (endpoint, handle) = server(vec![
        reply("latest:Media", media_response(1, 8000, &[], Some(0))),
        reply(
            "\"page0\":1",
            airing_response(vec![(true, vec![schedule.clone()]); 83]),
        ),
        reply(
            "\"page0\":84",
            airing_response(vec![(true, vec![schedule.clone()]); 17]),
        ),
        reply(
            "\"before\":6",
            airing_response(vec![(false, vec![schedule])]),
        ),
        reply(
            "\"after\":5",
            airing_response(vec![(false, vec![json!({"mediaId":2,"airingAt":6})])]),
        ),
    ]);
    let mut provider = fixture_provider(&endpoint);
    let page = provider
        .discover(Operation::Sync, Some(0), &Value::Null, 10)
        .unwrap();
    assert_eq!(dirty_keys(&page), vec!["media:1", "media:2"]);
    handle.join().unwrap();
}

#[test]
fn unsplittable_airing_window_fails_instead_of_skipping_events() {
    let schedule = json!({"mediaId":1,"airingAt":0});
    let (endpoint, handle) = server(vec![
        reply("latest:Media", media_response(1, 8000, &[], Some(0))),
        reply(
            "airingSchedules",
            airing_response(vec![(true, vec![schedule.clone()]); 83]),
        ),
        reply(
            "airingSchedules",
            airing_response(vec![(true, vec![schedule]); 17]),
        ),
    ]);
    let mut provider = fixture_provider(&endpoint);
    assert_eq!(
        provider
            .discover(Operation::Sync, Some(0), &Value::Null, 0)
            .unwrap_err()
            .to_string(),
        "AniList airing events exceed pagination depth within one second"
    );
    handle.join().unwrap();
}

#[test]
fn malformed_or_out_of_window_airings_fail() {
    for body in [
        json!({"data":{"page0":{"airingSchedules":[]}}}),
        json!({"data":{"page0":{"pageInfo":{"hasNextPage":true},"airingSchedules":[]}}}),
        airing_response(vec![(false, vec![json!({"mediaId":1,"airingAt":790399})])]),
        airing_response(vec![(false, vec![json!({"mediaId":1,"airingAt":2000011})])]),
        airing_response(vec![(false, vec![json!({"mediaId":1})])]),
        airing_response(vec![(true, vec![json!({"mediaId":1,"airingAt":2000000})])]),
    ] {
        let (endpoint, handle) = server(vec![
            reply("latest:Media", media_response(1, 8000, &[], Some(0))),
            reply("airingSchedules", body),
        ]);
        let mut provider = fixture_provider(&endpoint);
        assert!(
            provider
                .discover(Operation::Sync, Some(2000000), &Value::Null, 2000010)
                .is_err()
        );
        handle.join().unwrap();
    }
}

#[test]
fn invalid_checkpoints_cursors_and_time_windows_fail_without_requests() {
    let mut provider = fixture_provider("http://127.0.0.1:1");
    for checkpoint in [
        json!({"after":-1}),
        json!({"after":2147483648_u64}),
        json!({"after":1,"through":2}),
        json!({"unexpected":1}),
        json!("invalid"),
    ] {
        assert!(
            provider
                .restore(Operation::Sync, &BTreeMap::new(), &checkpoint)
                .is_err()
        );
    }
    for cursor in [
        json!({"after":-1}),
        json!({"after":2147483648_u64}),
        json!({"page":1}),
        json!({"after":10,"through":10}),
        json!({"after":10,"through":9}),
    ] {
        assert!(
            provider
                .discover(Operation::Bootstrap, None, &cursor, 100)
                .is_err()
        );
    }
    for (watermark, until) in [(-1, 100), (100, 99), (100, i32::MAX as i64)] {
        assert!(
            provider
                .discover(Operation::Sync, Some(watermark), &Value::Null, until)
                .is_err()
        );
    }
    assert!(
        provider
            .discover(Operation::Sync, Some(100), &json!({"page":1}), 200)
            .is_err()
    );
}

#[test]
fn bootstrap_packs_unknown_candidates_instead_of_rediscovering_live_ids() {
    let (endpoint, handle) = server(vec![reply(
        "\"ids0\":[8001,8002",
        media_response(8001, 8000, &[json!({"id":8001})], Some(8001)),
    )]);
    let mut provider = fixture_provider(&endpoint);
    let index = (1..=8000)
        .map(|media_id| {
            (
                format!("media:{media_id}"),
                Entry {
                    hash: "old".into(),
                    deleted: false,
                    metadata: Value::Null,
                },
            )
        })
        .collect();
    provider
        .restore(Operation::Bootstrap, &index, &Value::Null)
        .unwrap();
    let page = provider
        .discover(Operation::Bootstrap, None, &Value::Null, 100)
        .unwrap();
    assert_eq!(dirty_keys(&page), vec!["media:8001"]);
    assert!(page.next.is_none());
    assert_eq!(provider.checkpoint(), json!({"after":8001}));
    handle.join().unwrap();
}

#[test]
fn bootstrap_checks_old_gaps_and_tombstones_without_changing_reconciliation() {
    let (endpoint, handle) = server(vec![
        reply(
            "\"ids0\":[2,4,5",
            media_response(1, 8000, &[json!({"id":2}), json!({"id":4})], Some(4)),
        ),
        reply(
            "\"ids0\":[1,2,3",
            media_response(
                1,
                8000,
                &[
                    json!({"id":1}),
                    json!({"id":2}),
                    json!({"id":3}),
                    json!({"id":4}),
                ],
                Some(4),
            ),
        ),
    ]);
    let mut provider = fixture_provider(&endpoint);
    let index = (1..=3)
        .map(|media_id| {
            (
                format!("media:{media_id}"),
                Entry {
                    hash: "old".into(),
                    deleted: media_id == 2,
                    metadata: Value::Null,
                },
            )
        })
        .collect();
    provider
        .restore(Operation::Bootstrap, &index, &json!({"after":3}))
        .unwrap();
    let page = provider
        .discover(Operation::Bootstrap, None, &Value::Null, 100)
        .unwrap();
    assert_eq!(dirty_keys(&page), vec!["media:2", "media:4"]);
    assert!(page.next.is_none());
    provider
        .restore(Operation::Reconcile, &index, &Value::Null)
        .unwrap();
    let page = provider
        .discover(Operation::Reconcile, None, &Value::Null, 100)
        .unwrap();
    assert_eq!(
        dirty_keys(&page),
        vec!["media:1", "media:2", "media:3", "media:4"]
    );
    assert!(page.next.is_none());
    handle.join().unwrap();
}

#[test]
fn bootstrap_skips_known_tail_without_an_empty_graphql_request() {
    let (endpoint, handle) = server(vec![reply(
        "latest:Media",
        media_response(1, 8000, &[json!({"id":1})], Some(16001)),
    )]);
    let mut provider = fixture_provider(&endpoint);
    let index = (8001..=16001)
        .map(|media_id| {
            (
                format!("media:{media_id}"),
                Entry {
                    hash: "old".into(),
                    deleted: false,
                    metadata: Value::Null,
                },
            )
        })
        .collect();
    provider
        .restore(Operation::Bootstrap, &index, &Value::Null)
        .unwrap();
    let first = provider
        .discover(Operation::Bootstrap, None, &Value::Null, 100)
        .unwrap();
    assert_eq!(dirty_keys(&first), vec!["media:1"]);
    assert_eq!(first.next, Some(json!({"after":8000,"through":16001})));
    let last = provider
        .discover(Operation::Bootstrap, None, &first.next.unwrap(), 100)
        .unwrap();
    assert!(last.changes.is_empty());
    assert!(last.next.is_none());
    assert_eq!(provider.checkpoint(), json!({"after":16001}));
    handle.join().unwrap();
}

#[test]
fn reconciliation_defers_bounded_absence_checks_until_catalogue_completion() {
    let (endpoint, handle) = server(vec![
        reply(
            "latest:Media",
            media_response(1, 8000, &[json!({"id":1})], Some(8001)),
        ),
        reply(
            "\"ids0\":[8001]",
            media_response(8001, 1, &[json!({"id":8001})], None),
        ),
    ]);
    let mut provider = fixture_provider(&endpoint);
    let mut index: BTreeMap<_, _> = (1..=1002)
        .chain([8001])
        .map(|media_id| {
            (
                format!("media:{media_id}"),
                Entry {
                    hash: "old".into(),
                    deleted: false,
                    metadata: json!({}),
                },
            )
        })
        .collect();
    index.insert(
        "movie:1".into(),
        Entry {
            hash: "old".into(),
            deleted: false,
            metadata: json!({}),
        },
    );
    index.insert(
        "media:9000".into(),
        Entry {
            hash: "old".into(),
            deleted: true,
            metadata: json!({}),
        },
    );
    provider
        .restore(Operation::Reconcile, &index, &Value::Null)
        .unwrap();
    let first = provider
        .discover(Operation::Reconcile, None, &Value::Null, 100)
        .unwrap();
    assert_eq!(dirty_keys(&first), vec!["media:1"]);
    assert_eq!(first.next, Some(json!({"after":8000,"through":8001})));
    let last = provider
        .discover(Operation::Reconcile, None, &first.next.unwrap(), 100)
        .unwrap();
    assert_eq!(dirty_keys(&last), vec!["media:8001"]);
    assert_eq!(last.next, Some(json!({"absent":0})));
    let absent = provider
        .discover(Operation::Reconcile, None, &last.next.unwrap(), 100)
        .unwrap();
    assert_eq!(absent.changes.len(), 1000);
    assert_eq!(absent.next, Some(json!({"absent":1})));
    let tail = provider
        .discover(
            Operation::Reconcile,
            None,
            absent.next.as_ref().unwrap(),
            100,
        )
        .unwrap();
    assert_eq!(tail.changes.len(), 1);
    assert!(tail.next.is_none());
    assert!(
        dirty_keys(&absent)
            .into_iter()
            .chain(dirty_keys(&tail))
            .all(|key| key.starts_with("media:")
                && !["media:1", "media:8001", "media:9000"].contains(&key))
    );
    handle.join().unwrap();
}

#[test]
fn reconciliation_rejects_empty_catalogues_before_absence_checks() {
    let (endpoint, handle) = server(vec![reply(
        "latest:Media",
        media_response(1, 8000, &[], Some(0)),
    )]);
    let mut provider = fixture_provider(&endpoint);
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
fn catalogue_batches_disjoint_ids_and_crosses_empty_windows() {
    let (endpoint, handle) = server(vec![
        reply(
            "batch159:Page(page:1,perPage:50)",
            media_response(1, 8000, &[json!({"id":1}), json!({"id":8000})], Some(16001)),
        ),
        reply("\"ids0\":[8001,8002", media_response(8001, 8000, &[], None)),
        reply(
            "\"ids0\":[16001]",
            media_response(16001, 1, &[json!({"id":16001})], None),
        ),
    ]);
    let mut provider = fixture_provider(&endpoint);
    let first = provider
        .discover(Operation::Bootstrap, None, &Value::Null, 100)
        .unwrap();
    assert_eq!(dirty_keys(&first), vec!["media:1", "media:8000"]);
    assert_eq!(first.next, Some(json!({"after":8000,"through":16001})));
    let empty = provider
        .discover(Operation::Bootstrap, None, &first.next.unwrap(), 100)
        .unwrap();
    assert!(empty.changes.is_empty());
    assert_eq!(empty.next, Some(json!({"after":16000,"through":16001})));
    let last = provider
        .discover(Operation::Bootstrap, None, &empty.next.unwrap(), 100)
        .unwrap();
    assert_eq!(dirty_keys(&last), vec!["media:16001"]);
    assert!(last.next.is_none());
    assert_eq!(provider.checkpoint(), json!({"after":16001}));
    handle.join().unwrap();
}

#[test]
fn catalogue_covers_dense_windows_past_the_page_depth_limit() {
    let media: Vec<_> = (1..=8000).map(|media_id| json!({"id":media_id})).collect();
    let (endpoint, handle) = server(vec![reply(
        "batch159:Page",
        media_response(1, 8000, &media, Some(8001)),
    )]);
    let mut provider = fixture_provider(&endpoint);
    let page = provider
        .discover(Operation::Bootstrap, None, &Value::Null, 100)
        .unwrap();
    assert_eq!(page.changes.len(), 8000);
    assert_eq!(page.next, Some(json!({"after":8000,"through":8001})));
    handle.join().unwrap();
}

#[test]
fn catalogue_freezes_upper_boundary_without_losing_later_ids() {
    let (endpoint, handle) = server(vec![
        reply(
            "latest:Media",
            media_response(1, 8000, &[json!({"id":1}), json!({"id":2})], Some(1)),
        ),
        reply(
            "\"ids0\":[2,3",
            media_response(2, 8000, &[json!({"id":2})], Some(2)),
        ),
        reply("airingSchedules", airing_response(vec![(false, vec![])])),
    ]);
    let mut provider = fixture_provider(&endpoint);
    let first = provider
        .discover(Operation::Bootstrap, None, &Value::Null, 100)
        .unwrap();
    assert_eq!(dirty_keys(&first), vec!["media:1"]);
    assert_eq!(provider.checkpoint(), json!({"after":1}));
    let second = provider
        .discover(Operation::Sync, Some(100), &Value::Null, 200)
        .unwrap();
    assert_eq!(dirty_keys(&second), vec!["media:2"]);
    handle.join().unwrap();
}

#[test]
fn catalogue_rejects_partial_aliases_and_invalid_records() {
    let mut unexpected = media_response(1, 8000, &[], Some(6000));
    unexpected["data"]["batch0"]["media"] = json!([{"id":51}]);
    for body in [
        json!({"data":{"latest":{"id":6000},"batch0":{"media":[{"id":1}]}}}),
        unexpected,
        media_response(1, 8000, &[], None),
        media_response(1, 8000, &[json!({"id":1}), json!({"id":1})], Some(6000)),
    ] {
        let (endpoint, handle) = server(vec![reply("latest:Media", body)]);
        let mut provider = fixture_provider(&endpoint);
        assert!(
            provider
                .discover(Operation::Bootstrap, None, &Value::Null, 100)
                .is_err()
        );
        handle.join().unwrap();
    }
}

#[test]
fn payload_batches_are_bounded_and_use_disjoint_first_pages() {
    let media: Vec<_> = (1..=551).map(|media_id| json!({"id":media_id})).collect();
    let (endpoint, handle) = server(vec![
        reply(
            "batch10:Page(page:1,perPage:50)",
            media_response(1, 550, &media, None),
        ),
        reply("\"ids0\":[551]", media_response(551, 1, &media, None)),
    ]);
    let mut provider = fixture_provider(&endpoint);
    let keys: Vec<_> = (1..=551)
        .map(|media_id| format!("media:{media_id}"))
        .collect();
    assert_eq!(provider.fetch_many(&keys).unwrap().len(), 551);
    handle.join().unwrap();
}

#[test]
fn payloads_omit_updated_at_and_preserve_native_relationships() {
    let record = json!({"id":1,"type":"ANIME","relations":{"edges":[{"id":10,"relationType":"ADAPTATION","node":{"id":2,"type":"MANGA"}}]}});
    let (endpoint, handle) = server(vec![
        reply("source genres", json!({"data":{"Media":record}})),
        reply(
            "source genres",
            media_response(1, 1, std::slice::from_ref(&record), None),
        ),
    ]);
    let mut provider = fixture_provider(&endpoint);
    assert_eq!(provider.fetch("media:1").unwrap().unwrap(), record);
    assert_eq!(
        provider.fetch_many(&["media:1".into()]).unwrap()["media:1"],
        Some(record)
    );
    handle.join().unwrap();
}

#[test]
fn batch_omissions_require_individual_confirmation() {
    let (endpoint, handle) = server(vec![
        reply(
            "media(id_in:$ids0",
            media_response(1, 2, &[json!({"id":2})], None),
        ),
        Reply {
            status: 404,
            expected: "Media(id:$id)",
            body: serde_json::to_vec(&json!({"errors":[{"status":404}],"data":{"Media":null}}))
                .unwrap(),
        },
    ]);
    let mut provider = fixture_provider(&endpoint);
    let records = provider
        .fetch_many(&["media:1".into(), "media:2".into()])
        .unwrap();
    assert!(records["media:1"].is_none());
    assert_eq!(records["media:2"], Some(json!({"id":2})));
    handle.join().unwrap();
}

#[test]
fn omitted_batch_record_can_still_exist() {
    let (endpoint, handle) = server(vec![
        reply("media(id_in:$ids0", media_response(1, 1, &[], None)),
        reply("Media(id:$id)", json!({"data":{"Media":{"id":1}}})),
    ]);
    let mut provider = fixture_provider(&endpoint);
    assert_eq!(
        provider.fetch_many(&["media:1".into()]).unwrap()["media:1"],
        Some(json!({"id":1}))
    );
    handle.join().unwrap();
}

#[test]
fn batches_reject_errors_unexpected_ids_duplicates_and_missing_aliases() {
    for body in [
        json!({"errors":[{"message":"failed"}],"data":{"batch0":{"media":[]}}}),
        json!({"data":{"batch0":{"media":[{"id":999}]}}}),
        json!({"data":{"batch0":{"media":[{"id":1},{"id":1}]}}}),
        json!({"data":{"batch0":{}}}),
    ] {
        let (endpoint, handle) = server(vec![reply("media(id_in:$ids0", body)]);
        let mut provider = fixture_provider(&endpoint);
        assert!(provider.fetch_many(&["media:1".into()]).is_err());
        handle.join().unwrap();
    }
}

#[test]
fn complexity_rejection_fails_without_retry() {
    let (endpoint, handle) = server(vec![Reply {
        status: 400,
        expected: "batch1:Page",
        body: serde_json::to_vec(
            &json!({"errors":[{"message":"Max query complexity exceeded"}],"data":null}),
        )
        .unwrap(),
    }]);
    let mut provider = fixture_provider(&endpoint);
    let keys: Vec<_> = (1..=51)
        .map(|media_id| format!("media:{media_id}"))
        .collect();
    assert_eq!(
        provider.fetch_many(&keys).unwrap_err().to_string(),
        "AniList query complexity limit"
    );
    handle.join().unwrap();
}

#[test]
fn invalid_and_duplicate_fetch_keys_fail_without_requests() {
    let mut provider = fixture_provider("http://127.0.0.1:1");
    for key in [
        "movie:1",
        "media:0",
        "media:-1",
        "media:01",
        "media:2147483648",
    ] {
        assert!(provider.fetch(key).is_err());
        assert!(provider.fetch_many(&[key.into()]).is_err());
    }
    assert!(
        provider
            .fetch_many(&["media:1".into(), "media:1".into()])
            .is_err()
    );
    assert!(provider.fetch_many(&[]).unwrap().is_empty());
}

#[test]
fn requires_confirmed_missing_media_before_tombstoning() {
    let (endpoint, handle) = server(vec![
        Reply {
            status: 404,
            expected: "Media(id:$id)",
            body: serde_json::to_vec(&json!({"errors":[{"status":404}],"data":{"Media":null}}))
                .unwrap(),
        },
        Reply {
            status: 404,
            expected: "Media(id:$id)",
            body: serde_json::to_vec(&json!({"errors":[{"status":401}],"data":{"Media":null}}))
                .unwrap(),
        },
        reply(
            "Media(id:$id)",
            json!({"errors":[{"message":"failed"}],"data":{"Media":null}}),
        ),
        reply("Media(id:$id)", json!({"data":{"Media":{"id":999}}})),
    ]);
    let mut provider = fixture_provider(&endpoint);
    assert!(provider.fetch("media:1").unwrap().is_none());
    assert!(provider.fetch("media:2").is_err());
    assert!(provider.fetch("media:3").is_err());
    assert!(provider.fetch("media:4").is_err());
    handle.join().unwrap();
}

#[test]
#[ignore = "Requires the live AniList API"]
fn live_catalogue_payload_and_airing_queries() {
    let now = chrono::Utc::now().timestamp();
    let mut provider = AniList::new().unwrap();
    let catalogue = provider
        .discover(Operation::Bootstrap, None, &Value::Null, now)
        .unwrap();
    assert!(!catalogue.changes.is_empty());
    let keys: Vec<_> = dirty_keys(&catalogue)
        .into_iter()
        .take(550)
        .map(str::to_owned)
        .collect();
    let records = provider.fetch_many(&keys).unwrap();
    assert_eq!(records.len(), keys.len());
    assert!(
        records
            .values()
            .flatten()
            .all(|record| record.get("updatedAt").is_none())
    );
    provider
        .restore(
            Operation::Sync,
            &BTreeMap::new(),
            &json!({"after":i32::MAX}),
        )
        .unwrap();
    let airings = provider
        .discover(Operation::Sync, Some(now - 86400), &Value::Null, now)
        .unwrap();
    assert!(airings.next.is_none());
    println!(
        "Live AniList: {} catalogue IDs, {} payload results, {} airing refresh IDs",
        catalogue.changes.len(),
        records.len(),
        airings.changes.len()
    );
}
