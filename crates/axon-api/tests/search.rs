//! DB-gated HTTP integration tests for `GET /v1/search` (M9b).
//!
//! The search *engine* (Tantivy ranking/filters) is unit-tested in `axon-search`;
//! these tests cover the **HTTP layer**: query parsing, the `SearchQuery` port
//! contract (filters/limit/offset passthrough), hydration of hits into `EventDto`
//! from the store, pagination cursors, and error → status mapping. They use a
//! [`StubSearchQuery`] (canned hits) against a real `Store`, so they need a
//! database and are `#[ignore]`d by default — run like the other axon-api tests:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-api -- --ignored
//! ```

mod common;

use std::sync::Arc;

use axon_api::{AppState, SearchQuery};
use axon_store::{NewEvent, Store};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    StubDeviceList, StubLifecycle, StubMediaProxy, StubSearchQuery, StubSender, StubTokenVerifier,
    StubTrust, StubVerification, TEST_TOKEN,
};
use serde_json::Value;
use tower::ServiceExt; // for `oneshot`
use uuid::Uuid;

async fn store() -> Store {
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
    Store::connect(&url, 5).await.expect("connect + migrate")
}

/// Build the search router over `store` with the given (optional) search port and
/// throwaway stubs for the ports the search endpoint doesn't touch.
fn search_app(store: Store, search: Option<Arc<dyn SearchQuery>>) -> axum::Router {
    let (live, _rx) = tokio::sync::broadcast::channel(16);
    axon_api::router(AppState::new(
        store,
        live,
        Arc::new(StubSender::ok("$unused:localhost")),
        Arc::new(StubLifecycle::ok(Uuid::nil())),
        Arc::new(StubVerification::ok("$unused-flow")),
        Arc::new(StubTrust::ok()),
        Arc::new(StubDeviceList::ok()),
        Arc::new(StubTokenVerifier::ok()),
        Arc::new(StubMediaProxy),
        search,
    ))
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.expect("router response");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, body)
}

/// Seed an account + a message event, returning `(account_id, event_id)`.
async fn seed_message(store: &Store, room_id: &str, ts: i64, body: &str) -> (Uuid, String) {
    let user = format!("@search-{}:localhost", Uuid::new_v4());
    let account = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("seed account");
    let event_id = format!("$evt-{}:localhost", Uuid::new_v4());
    let content = serde_json::json!({ "msgtype": "m.text", "body": body });
    store
        .upsert_event(&NewEvent {
            event_id: &event_id,
            room_id,
            account_id: account.account_id,
            sender: "@alice:localhost",
            origin_ts: ts,
            event_type: "m.room.message",
            content: Some(content.clone()),
            raw_event: serde_json::json!({ "type": "m.room.message", "content": content }),
            megolm_session_id: None,
            redacts: None,
            relates_to: None,
            decrypted_body_text: Some(body),
        })
        .await
        .expect("insert message");
    (account.account_id, event_id)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn search_hydrates_hits_in_rank_order() {
    let store = store().await;
    let room = format!("!room-{}:localhost", Uuid::new_v4());
    let (a1, e1) = seed_message(&store, &room, 1000, "the quick brown fox").await;
    let (a2, e2) = seed_message(&store, &room, 2000, "a slow green turtle").await;

    // The stub returns e1 first (higher score), then e2.
    let stub = Arc::new(StubSearchQuery::returning(
        vec![
            StubSearchQuery::hit(a1, &e1, 9.5),
            StubSearchQuery::hit(a2, &e2, 1.2),
        ],
        2,
    ));
    let app = search_app(store.clone(), Some(stub.clone()));

    let (status, body) = get(&app, "/v1/search?q=anything").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let data = &body["data"];
    assert_eq!(data["total"], 2);
    assert!(data["next_cursor"].is_null(), "single page → no cursor");
    let results = data["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);
    // Order is preserved (rank order from the index), and events are hydrated.
    assert_eq!(results[0]["event"]["event_id"], e1);
    assert_eq!(results[0]["event"]["body"], "the quick brown fox");
    assert_eq!(results[0]["score"], 9.5);
    assert_eq!(results[1]["event"]["event_id"], e2);
    assert_eq!(results[1]["event"]["body"], "a slow green turtle");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn search_passes_filters_and_pagination_to_port() {
    let store = store().await;
    let account_id = Uuid::new_v4();
    let stub = Arc::new(StubSearchQuery::returning(vec![], 0));
    let app = search_app(store.clone(), Some(stub.clone()));

    // `limit` over the cap clamps to 200; `cursor` decodes to an offset.
    // The first page's cursor encodes offset 50 (base64url of "50" = "NTA").
    let uri = format!(
        "/v1/search?q=hello%20world&account_id={account_id}&room_id=!r:localhost\
         &sender=@bob:localhost&from=100&to=900&sort=newest&limit=500&cursor=NTA"
    );
    let (status, _body) = get(&app, &uri).await;
    assert_eq!(status, StatusCode::OK);

    let calls = stub.calls();
    assert_eq!(calls.len(), 1);
    let p = &calls[0];
    assert_eq!(p.text, "hello world");
    assert_eq!(p.account_id, Some(account_id));
    assert_eq!(p.room_id.as_deref(), Some("!r:localhost"));
    assert_eq!(p.sender.as_deref(), Some("@bob:localhost"));
    assert_eq!(p.from_ts, Some(100));
    assert_eq!(p.to_ts, Some(900));
    assert_eq!(p.sort, axon_api::SearchSort::Newest);
    assert_eq!(p.limit, 200, "limit clamps to the max");
    assert_eq!(p.offset, 50, "cursor decodes to the offset");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn search_pagination_emits_next_cursor() {
    let store = store().await;
    let room = format!("!room-{}:localhost", Uuid::new_v4());
    let (a1, e1) = seed_message(&store, &room, 1000, "paginate me").await;

    // total (5) > offset (0) + returned (1) → there is a next page.
    let stub = Arc::new(StubSearchQuery::returning(
        vec![StubSearchQuery::hit(a1, &e1, 3.0)],
        5,
    ));
    let app = search_app(store.clone(), Some(stub.clone()));

    let (status, body) = get(&app, "/v1/search?q=paginate&limit=1").await;
    assert_eq!(status, StatusCode::OK);
    let next = body["data"]["next_cursor"].as_str().expect("a next cursor");

    // Re-issue with the returned cursor; it decodes back to offset 1 at the port.
    let (status, _body) = get(
        &app,
        &format!("/v1/search?q=paginate&limit=1&cursor={next}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stub.calls().last().expect("a call").offset, 1);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn search_skips_hit_whose_row_is_gone() {
    let store = store().await;
    let room = format!("!room-{}:localhost", Uuid::new_v4());
    let (a1, e1) = seed_message(&store, &room, 1000, "present body").await;

    // Two hits: one real, one that no longer exists in the store (index/DB race).
    let stub = Arc::new(StubSearchQuery::returning(
        vec![
            StubSearchQuery::hit(a1, &e1, 5.0),
            StubSearchQuery::hit(a1, "$ghost:localhost", 4.0),
        ],
        2,
    ));
    let app = search_app(store.clone(), Some(stub));

    let (status, body) = get(&app, "/v1/search?q=present").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a missing row is skipped, not a 500"
    );
    let data = &body["data"];
    // total still reflects the index's count; only the hydratable hit is returned.
    assert_eq!(data["total"], 2);
    let results = data["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["event"]["event_id"], e1);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn search_missing_or_empty_query_is_400_without_filters() {
    let store = store().await;
    let stub = Arc::new(StubSearchQuery::returning(vec![], 0));
    let app = search_app(store.clone(), Some(stub.clone()));

    let (status, body) = get(&app, "/v1/search").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "missing q: {body}");

    let (status, _body) = get(&app, "/v1/search?q=").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "empty q");

    let (status, _body) = get(&app, "/v1/search?q=%20%20").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "whitespace-only q");

    assert!(stub.calls().is_empty(), "the port is never reached");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn search_empty_query_is_allowed_with_a_filter() {
    let store = store().await;
    let stub = Arc::new(StubSearchQuery::returning(vec![], 0));
    let app = search_app(store.clone(), Some(stub.clone()));

    let (status, body) = get(&app, "/v1/search?sender=jamie").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let calls = stub.calls();
    assert_eq!(calls.len(), 1);
    let p = &calls[0];
    assert_eq!(p.text, "");
    assert_eq!(p.sender.as_deref(), Some("jamie"));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn search_empty_query_with_only_pagination_params_is_400() {
    let store = store().await;
    let stub = Arc::new(StubSearchQuery::returning(vec![], 0));
    let app = search_app(store.clone(), Some(stub.clone()));

    let (status, body) = get(&app, "/v1/search?q=&limit=25").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "empty query with no narrowing filter: {body}"
    );
    assert!(stub.calls().is_empty(), "the port is never reached");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn search_invalid_cursor_is_400() {
    let store = store().await;
    let stub = Arc::new(StubSearchQuery::returning(vec![], 0));
    let app = search_app(store.clone(), Some(stub.clone()));

    let (status, _body) = get(&app, "/v1/search?q=hello&cursor=not-a-cursor!!!").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(stub.calls().is_empty(), "a bad cursor short-circuits");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn search_huge_offset_cursor_is_400() {
    let store = store().await;
    let stub = Arc::new(StubSearchQuery::returning(vec![], 0));
    let app = search_app(store.clone(), Some(stub.clone()));

    // A well-formed offset cursor (base64url of "100001") that decodes to an offset
    // one past the handler's `MAX_OFFSET` cap. It must be refused before the port —
    // a forged huge offset is a skip-N resource-amplification vector at the index.
    let (status, body) = get(&app, "/v1/search?q=hello&cursor=MTAwMDAx").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "out-of-range offset: {body}"
    );
    assert!(
        stub.calls().is_empty(),
        "an over-cap cursor never reaches the search port"
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn search_bad_query_is_400() {
    let store = store().await;
    let stub = Arc::new(StubSearchQuery::bad_query("unbalanced quote"));
    let app = search_app(store.clone(), Some(stub));

    let (status, body) = get(&app, "/v1/search?q=%22unterminated").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn search_disabled_is_503() {
    let store = store().await;
    // No search port → search is disabled.
    let app = search_app(store.clone(), None);

    let (status, body) = get(&app, "/v1/search?q=hello").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "service_unavailable");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn search_sort_defaults_validates_and_does_not_count_as_a_filter() {
    let store = store().await;
    let stub = Arc::new(StubSearchQuery::returning(vec![], 0));
    let app = search_app(store, Some(stub.clone()));
    for (suffix, expected) in [
        ("", axon_api::SearchSort::Relevance),
        ("&sort=relevance", axon_api::SearchSort::Relevance),
        ("&sort=newest", axon_api::SearchSort::Newest),
        ("&sort=oldest", axon_api::SearchSort::Oldest),
    ] {
        let (status, body) = get(&app, &format!("/v1/search?q=needle{suffix}")).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(stub.calls().last().unwrap().sort, expected);
    }
    let calls = stub.calls().len();
    for uri in ["/v1/search?q=needle&sort=invalid", "/v1/search?sort=newest"] {
        let (status, body) = get(&app, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    }
    assert_eq!(
        stub.calls().len(),
        calls,
        "invalid requests never reach the index"
    );
}
