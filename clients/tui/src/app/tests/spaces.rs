//! Exercise the HTTP adapters using the same loopback fixture as room tests.
use super::support::*;
use crate::api::{ApiError, AxonClient};
use uuid::Uuid;

#[tokio::test]
async fn space_http_reads_and_writes_consume_the_actual_api_envelopes() {
    let url = spawn_api_stub(vec![
        serde_json::json!({ "data": { "room_id": "!space/with-path:srv", "children": [{ "room_id": "!a:srv", "parent_room_id": "!space/with-path:srv", "via": ["srv"], "suggested": false, "order": "a", "name": "A", "room_type": null }], "next_cursor": null, "total_children": 1 } }).to_string(),
        serde_json::json!({ "data": { "key": "space_order", "value": { "spaces": [] }, "updated_at": "2026-09-30T00:00:00Z" } }).to_string(),
        serde_json::json!({ "data": { "updated_at": "2026-09-30T00:00:00Z" } }).to_string(),
    ]).await;
    let client = AxonClient::new(url, None);
    let (page, bytes) = client
        .space_hierarchy(Uuid::nil(), "!space/with-path:srv", None, 16 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(page.children[0].room_id, "!a:srv");
    assert!(bytes > 0);
    assert_eq!(
        client.space_order().await.unwrap().value,
        serde_json::json!({ "spaces": [] })
    );
    client
        .put_space_order(Uuid::new_v4(), &[format!("{}/!space:srv", Uuid::nil())])
        .await
        .unwrap();
}

#[tokio::test]
async fn space_hierarchy_respects_remaining_walk_bytes_before_decoding() {
    let url = spawn_api_stub(vec!["x".repeat(1025)]).await;
    let error = AxonClient::new(url, None)
        .space_hierarchy(Uuid::nil(), "!space:srv", None, 1024)
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::Request(message) if message.contains("size limit")));
}

#[tokio::test]
async fn overlarge_space_order_is_rejected_before_any_request() {
    let client = AxonClient::new("http://127.0.0.1:1".to_owned(), None);
    let error = client
        .put_space_order(Uuid::nil(), &["x".repeat(64 * 1024)])
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::Request(message) if message.contains("64 KiB")));
}
