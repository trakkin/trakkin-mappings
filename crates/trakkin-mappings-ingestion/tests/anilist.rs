mod common;

use common::{Reply, reply, server};
use serde_json::{Value, json};
use trakkin_mappings_ingestion::{Provider, providers::AniList};

#[test]
fn uses_fixed_maximum_changes_and_payload_pages() {
    let mut payload_pages = serde_json::Map::new();
    let media: Vec<_> = (1..=501).map(|media_id| json!({"id":media_id})).collect();
    for (offset, records) in media.chunks(50).enumerate() {
        let name = if offset == 0 {
            "Page".to_owned()
        } else {
            format!("page{offset}")
        };
        payload_pages.insert(
            name,
            json!({"pageInfo":{"hasNextPage":offset < 10},"media":records}),
        );
    }
    let (endpoint, handle) = server(vec![
        reply(
            "page82:Page(page:$page82",
            json!({"data":{"Page":{"pageInfo":{"hasNextPage":false},"media":[]}}}),
        ),
        reply("page10:Page(page:$page10", json!({"data":payload_pages})),
    ]);
    let mut provider = AniList::with_endpoint(&endpoint).unwrap();
    assert!(
        provider
            .discover_changes(100, &Value::Null, 200)
            .unwrap()
            .next
            .is_none()
    );
    let keys: Vec<_> = (1..=501)
        .map(|media_id| format!("media:{media_id}"))
        .collect();
    assert_eq!(provider.fetch_many(&keys).unwrap().len(), 501);
    handle.join().unwrap();
}

#[test]
fn fetches_multiple_payload_pages_in_one_request() {
    let first: Vec<_> = (1..=50).map(|media_id| json!({"id":media_id})).collect();
    let (endpoint, handle) = server(vec![reply(
        "page1:Page(page:$page1",
        json!({"data":{
            "Page":{"pageInfo":{"hasNextPage":true},"media":first},
            "page1":{"pageInfo":{"hasNextPage":false},"media":[{"id":51}]}
        }}),
    )]);
    let mut provider = AniList::with_endpoint(&endpoint).unwrap();
    let keys: Vec<_> = (1..=51)
        .map(|media_id| format!("media:{media_id}"))
        .collect();
    assert_eq!(provider.fetch_many(&keys).unwrap().len(), 51);
    handle.join().unwrap();
}

#[test]
fn relationships_preserve_native_type_and_cross_media_targets() {
    let record = json!({"id":1,"type":"ANIME","relations":{"edges":[{"id":10,"relationType":"ADAPTATION","node":{"id":2,"type":"MANGA"}}]}});
    let (endpoint, handle) = server(vec![reply(
        "relations{edges{id relationType(version:3) node{id type}}}",
        json!({"data":{"Media":record}}),
    )]);
    let mut provider = AniList::with_endpoint(&endpoint).unwrap();
    assert_eq!(provider.fetch("media:1").unwrap().unwrap(), record);
    handle.join().unwrap();
}

#[test]
fn catalogue_batches_ids_and_crosses_empty_windows() {
    let (endpoint, handle) = server(vec![
        reply(
            "page98:Page(page:$page98",
            json!({"data":{
                "latest":{"id":10001},
                "Page":{"pageInfo":{"hasNextPage":true},"media":[{"id":1}]},
                "page1":{"pageInfo":{"hasNextPage":false},"media":[{"id":5000}]}
            }}),
        ),
        reply(
            "\"ids\":[5001,5002",
            json!({"data":{
                "latest":{"id":10001},
                "Page":{"pageInfo":{"hasNextPage":false},"media":[]}
            }}),
        ),
        reply(
            "\"ids\":[10001,10002",
            json!({"data":{
                "latest":{"id":10001},
                "Page":{"pageInfo":{"hasNextPage":false},"media":[{"id":10001}]}
            }}),
        ),
    ]);
    let mut provider = AniList::with_endpoint(&endpoint).unwrap();
    let first = provider.enumerate(&Value::Null, 100).unwrap();
    assert_eq!(first.changes.len(), 2);
    assert_eq!(first.next, Some(json!({"after":5000})));
    let empty = provider.enumerate(&first.next.unwrap(), 100).unwrap();
    assert!(empty.changes.is_empty());
    assert_eq!(empty.next, Some(json!({"after":10000})));
    let last = provider.enumerate(&empty.next.unwrap(), 100).unwrap();
    assert_eq!(last.changes.len(), 1);
    assert!(last.next.is_none());
    handle.join().unwrap();
}

#[test]
fn complexity_rejection_fails_without_retry() {
    let (endpoint, handle) = server(vec![Reply {
        status: 400,
        expected: "page1:Page",
        body: serde_json::to_vec(
            &json!({"errors":[{"message":"Max query complexity exceeded"}],"data":null}),
        )
        .unwrap(),
    }]);
    let mut provider = AniList::with_endpoint(&endpoint).unwrap();
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
fn catalogue_rejects_partial_aliases_and_out_of_window_ids() {
    for body in [
        json!({"data":{"latest":{"id":6000},"Page":{"pageInfo":{"hasNextPage":true},"media":[{"id":1}]}}}),
        json!({"data":{"latest":{"id":6000},"Page":{"pageInfo":{"hasNextPage":false},"media":[{"id":5001}]}}}),
    ] {
        let (endpoint, handle) = server(vec![reply("latest:Media", body)]);
        let mut provider = AniList::with_endpoint(&endpoint).unwrap();
        assert!(provider.enumerate(&Value::Null, 100).is_err());
        handle.join().unwrap();
    }
}

#[test]
fn batches_payloads_and_confirms_omissions() {
    let (endpoint, handle) = server(vec![
        reply(
            "media(id_in:$ids",
            json!({"data":{"Page":{"pageInfo":{"hasNextPage":false},"media":[{"id":2,"title":{"romaji":"Fixture"}}]}}}),
        ),
        Reply {
            status: 404,
            body: serde_json::to_vec(&json!({"errors":[{"status":404}],"data":{"Media":null}}))
                .unwrap(),
            expected: "Media(id:$id)",
        },
    ]);
    let mut provider = AniList::with_endpoint(&endpoint).unwrap();
    let records = provider
        .fetch_many(&["media:1".into(), "media:2".into()])
        .unwrap();
    assert!(records["media:1"].is_none());
    assert_eq!(
        records["media:2"].as_ref().unwrap()["title"]["romaji"],
        "Fixture"
    );
    handle.join().unwrap();
}

#[test]
fn batch_rejects_errors_unexpected_ids_and_duplicates() {
    for body in [
        json!({"errors":[{"message":"failed"}],"data":{"Page":{"media":[]}}}),
        json!({"data":{"Page":{"pageInfo":{"hasNextPage":false},"media":[{"id":999}]}}}),
        json!({"data":{"Page":{"pageInfo":{"hasNextPage":false},"media":[{"id":1},{"id":1}]}}}),
    ] {
        let (endpoint, handle) = server(vec![reply("media(id_in:$ids", body)]);
        let mut provider = AniList::with_endpoint(&endpoint).unwrap();
        assert!(provider.fetch_many(&["media:1".into()]).is_err());
        handle.join().unwrap();
    }
}

#[test]
fn complete_batch_accepts_advisory_successor() {
    let (endpoint, handle) = server(vec![reply(
        "media(id_in:$ids",
        json!({"data":{"Page":{"pageInfo":{"hasNextPage":true},"media":[{"id":1}]}}}),
    )]);
    let mut provider = AniList::with_endpoint(&endpoint).unwrap();
    assert_eq!(
        provider.fetch_many(&["media:1".into()]).unwrap()["media:1"]
            .as_ref()
            .unwrap()["id"],
        1
    );
    handle.join().unwrap();
}

#[test]
fn incomplete_batch_follows_successor() {
    let (endpoint, handle) = server(vec![
        reply(
            "\"page\":1",
            json!({"data":{"Page":{"pageInfo":{"hasNextPage":true},"media":[{"id":1}]}}}),
        ),
        reply(
            "\"page\":2",
            json!({"data":{"Page":{"pageInfo":{"hasNextPage":false},"media":[{"id":2}]}}}),
        ),
    ]);
    let mut provider = AniList::with_endpoint(&endpoint).unwrap();
    assert_eq!(
        provider
            .fetch_many(&["media:1".into(), "media:2".into()])
            .unwrap()
            .len(),
        2
    );
    handle.join().unwrap();
}

#[test]
fn stops_at_overlap_and_rejects_graphql_errors() {
    let (endpoint, handle) = server(vec![
        reply(
            "UPDATED_AT_DESC",
            json!({"data":{"Page":{"pageInfo":{"hasNextPage":true},"media":[{"id":1,"updatedAt":200000},{"id":2,"updatedAt":113600},{"id":3,"updatedAt":113599}]}}}),
        ),
        reply(
            "Media(id:$id)",
            json!({"errors":[{"message":"failure"}],"data":{"Media":null}}),
        ),
    ]);
    let mut provider = AniList::with_endpoint(&endpoint).unwrap();
    assert!(provider.restart_changes());
    let page = provider
        .discover_changes(200000, &Value::Null, 300000)
        .unwrap();
    assert_eq!(page.changes.len(), 2);
    assert!(page.next.is_none());
    assert!(provider.fetch("media:1").is_err());
    handle.join().unwrap();
}

#[test]
fn malformed_catalogue_is_not_a_successful_empty_scan() {
    let (endpoint, handle) = server(vec![reply(
        "pageInfo",
        json!({"data":{"Page":{"pageInfo":{"hasNextPage":false}}}}),
    )]);
    let mut provider = AniList::with_endpoint(&endpoint).unwrap();
    assert!(provider.enumerate(&Value::Null, 100).is_err());
    handle.join().unwrap();
}

#[test]
fn requires_confirmed_missing_media_before_tombstoning() {
    let (endpoint, handle) = server(vec![
        Reply {
            status: 404,
            body: serde_json::to_vec(&json!({"errors":[{"status":404}],"data":{"Media":null}}))
                .unwrap(),
            expected: "Media(id:$id)",
        },
        Reply {
            status: 404,
            body: serde_json::to_vec(&json!({"errors":[{"status":401}],"data":{"Media":null}}))
                .unwrap(),
            expected: "Media(id:$id)",
        },
    ]);
    let mut provider = AniList::with_endpoint(&endpoint).unwrap();
    assert!(provider.fetch("media:1").unwrap().is_none());
    assert!(provider.fetch("media:2").is_err());
    handle.join().unwrap();
}
