//! Exercise the HTTP adapters using the same loopback fixture as room tests.
use super::support::*;
use crate::api::{ApiError, AxonClient};
use uuid::Uuid;

#[tokio::test]
async fn space_http_reads_and_writes_consume_the_actual_api_envelopes() {
    let url = spawn_api_stub(vec![
        serde_json::json!({ "data": [{ "room_id": "!a:srv", "via": ["srv"], "suggested": false, "order": "a", "name": "A", "room_type": null }] }).to_string(),
        serde_json::json!({ "data": { "key": "space_order", "value": { "spaces": [] }, "updated_at": "2026-09-30T00:00:00Z" } }).to_string(),
        serde_json::json!({ "data": { "updated_at": "2026-09-30T00:00:00Z" } }).to_string(),
    ]).await;
    let client = AxonClient::new(url, None);
    let children = client
        .space_children(Uuid::nil(), "!space/with-path:srv")
        .await
        .unwrap();
    assert_eq!(children[0].room_id, "!a:srv");
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
async fn space_child_count_and_response_bytes_are_bounded() {
    let url = spawn_api_stub(vec![
        serde_json::json!({ "data": vec![serde_json::json!({ "room_id": "!a:srv" }); 1001] })
            .to_string(),
    ])
    .await;
    let error = AxonClient::new(url, None)
        .space_children(Uuid::nil(), "!space:srv")
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::Request(message) if message.contains("1000 children")));
    let url = spawn_api_stub(vec![
        serde_json::json!({ "data": [{ "room_id": "x".repeat(1024 * 1024) }] }).to_string(),
    ])
    .await;
    let error = AxonClient::new(url, None)
        .space_children(Uuid::nil(), "!space:srv")
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
