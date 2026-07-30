//! DB-gated HTTP integration tests: drive the real router against Postgres.
//!
//! These seed a few rows through the `Store` API, then exercise the `/v1/`
//! handlers via `tower`'s `oneshot` and assert on status codes and the JSON
//! envelope. Like the store tests they need a database and are `#[ignore]`d by
//! default:
//!
//! ```sh
//! docker compose up -d postgres
//! # 5432 is the default; use your compose host port (e.g. 5433 if 5432 is taken).
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-api -- --ignored
//! ```
//!
//! Every `/v1/` route requires a bearer token (M7b); the router here is built
//! with a [`StubTokenVerifier`] that accepts [`TEST_TOKEN`], and the request
//! helpers attach it. The auth gate itself is exercised by the
//! `auth_gate_*` tests below.

mod common;

use std::sync::Arc;

use axon_api::{AccountLifecycle, AppState, MediaProxy, RedecryptUtdsStats};
use axon_store::{AccountState, NewEvent, RoomStateUpsert, Store};
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use common::{
    ConfiguredMediaProxy, DeleteOutcome, ImportTokenCall, LoginCall, LoginOutcome, LogoutOutcome,
    MediaOutcome, RecoverOutcome, RedecryptOutcome, StubDeviceList, StubLifecycle, StubMediaProxy,
    StubMemberProfiles, StubSender, StubSyncState, StubTokenVerifier, StubTrust, StubVerification,
    VerifyCall, VerifyOutcome, TEST_TOKEN,
};
use serde_json::{json, Value};
use tower::ServiceExt; // for `oneshot`
use uuid::Uuid;

async fn store() -> Store {
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
    Store::connect(&url, 5).await.expect("connect + migrate")
}

/// The default `Authorization` header value the helpers send.
fn bearer() -> String {
    format!("Bearer {TEST_TOKEN}")
}

/// Lowest-level request driver, shared by [`request_parts`],
/// [`request_text_parts`], and [`get_media_bytes`]: builds the request
/// (optional JSON body, optional `Authorization` header, optional one extra
/// header), runs it, and returns the raw response parts — `(status,
/// response headers, raw body bytes)`.
async fn send_request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    auth: Option<&str>,
    extra_header: Option<(&str, &str)>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(value) = auth {
        builder = builder.header("authorization", value);
    }
    if let Some((name, value)) = extra_header {
        builder = builder.header(name, value);
    }
    let req = match &body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.expect("request");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, headers, bytes.to_vec())
}

/// Optional JSON body, optional full `Authorization` header value (the auth
/// tests pass a wrong value or `None`). Returns `(status, response headers,
/// parsed body)`; the body is `Null` for empty responses (e.g. a 204 or a
/// pre-handler rejection).
async fn request_parts(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    auth: Option<&str>,
) -> (StatusCode, HeaderMap, Value) {
    let (status, headers, bytes) = send_request(app, method, uri, body, auth, None).await;
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, headers, json)
}

/// As [`request_parts`], but returns the raw body bytes decoded as UTF-8 text.
/// Used for non-JSON responses such as the browser fallback page.
async fn request_text_parts(
    app: &axum::Router,
    method: &str,
    uri: &str,
    auth: Option<&str>,
) -> (StatusCode, HeaderMap, String) {
    let (status, headers, bytes) = send_request(app, method, uri, None, auth, None).await;
    let body = String::from_utf8(bytes).expect("utf-8 body");
    (status, headers, body)
}

/// As [`request_parts`], dropping the response headers — the common case.
async fn request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    auth: Option<&str>,
) -> (StatusCode, Value) {
    let (status, _headers, json) = request_parts(app, method, uri, body, auth).await;
    (status, json)
}

/// Authenticated `GET`.
async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    request(app, "GET", uri, None, Some(&bearer())).await
}

/// Authenticated `GET` returning raw response bytes (not JSON) plus headers —
/// for the media/thumbnail routes' binary `200`/`206`/`304` responses, with an
/// optional `If-None-Match` request header for conditional-GET tests.
async fn get_media_bytes(
    app: &axum::Router,
    uri: &str,
    if_none_match: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    send_request(
        app,
        "GET",
        uri,
        None,
        Some(&bearer()),
        if_none_match.map(|etag| ("if-none-match", etag)),
    )
    .await
}

async fn insert_message(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    ts: i64,
    body: &str,
) -> String {
    let event_id = format!("$evt-{}:localhost", Uuid::new_v4());
    let content = json!({ "msgtype": "m.text", "body": body });
    store
        .upsert_event(&NewEvent {
            event_id: &event_id,
            room_id,
            account_id,
            sender: "@alice:localhost",
            origin_ts: ts,
            event_type: "m.room.message",
            content: Some(content.clone()),
            raw_event: json!({ "type": "m.room.message", "content": content }),
            megolm_session_id: None,
            redacts: None,
            relates_to: None,
            decrypted_body_text: Some(body),
        })
        .await
        .expect("insert message");
    event_id
}

/// Insert an event carrying an explicit `relates_to` (and sender) — the shape the
/// M8 aggregation reads resolve over. The text body, if present, is lifted into
/// `decrypted_body_text` like the live ingestion path. Returns its event_id.
#[allow(clippy::too_many_arguments)]
async fn insert_relation(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    sender: &str,
    ts: i64,
    event_type: &str,
    content: Value,
    relates_to: Value,
) -> String {
    let event_id = format!("$rel-{}:localhost", Uuid::new_v4());
    let body = content
        .get("body")
        .and_then(|b| b.as_str())
        .map(str::to_owned);
    store
        .upsert_event(&NewEvent {
            event_id: &event_id,
            room_id,
            account_id,
            sender,
            origin_ts: ts,
            event_type,
            content: Some(content.clone()),
            raw_event: json!({ "type": event_type, "content": content }),
            megolm_session_id: None,
            redacts: None,
            relates_to: Some(relates_to),
            decrypted_body_text: body.as_deref(),
        })
        .await
        .expect("insert relation");
    event_id
}

/// Redact `target` with an `m.room.redaction` event.
async fn insert_redaction(store: &Store, account_id: Uuid, room_id: &str, ts: i64, target: &str) {
    let event_id = format!("$red-{}:localhost", Uuid::new_v4());
    store
        .upsert_event(&NewEvent {
            event_id: &event_id,
            room_id,
            account_id,
            sender: "@alice:localhost",
            origin_ts: ts,
            event_type: "m.room.redaction",
            content: Some(json!({})),
            raw_event: json!({ "type": "m.room.redaction", "redacts": target }),
            megolm_session_id: None,
            redacts: Some(target),
            relates_to: None,
            decrypted_body_text: None,
        })
        .await
        .expect("insert redaction");
}

/// Build the read-API router over `store` with throwaway stubs for the ports the
/// read endpoints don't touch (sender, lifecycle, verify, trust, media).
fn read_app(store: Store) -> axum::Router {
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
        None,
    ))
}

/// Build a router whose lifecycle routes are backed by `lifecycle`, gated by a
/// [`StubTokenVerifier`] that accepts [`TEST_TOKEN`]. The sender and live bus are
/// unused by these paths.
fn lifecycle_app(store: Store, lifecycle: Arc<dyn AccountLifecycle>) -> axum::Router {
    let (live, _rx) = tokio::sync::broadcast::channel(16);
    axon_api::router(AppState::new(
        store,
        live,
        Arc::new(StubSender::ok("$unused:localhost")),
        lifecycle,
        Arc::new(StubVerification::ok("$unused-flow")),
        Arc::new(StubTrust::ok()),
        Arc::new(StubDeviceList::ok()),
        Arc::new(StubTokenVerifier::ok()),
        Arc::new(StubMediaProxy),
        None,
    ))
}

/// Build a router whose verify routes are backed by `verify` (other ports unused).
fn verify_app(store: Store, verify: Arc<dyn axon_api::VerificationService>) -> axum::Router {
    let (live, _rx) = tokio::sync::broadcast::channel(16);
    axon_api::router(AppState::new(
        store,
        live,
        Arc::new(StubSender::ok("$unused:localhost")),
        Arc::new(StubLifecycle::ok(Uuid::nil())),
        verify,
        Arc::new(StubTrust::ok()),
        Arc::new(StubDeviceList::ok()),
        Arc::new(StubTokenVerifier::ok()),
        Arc::new(StubMediaProxy),
        None,
    ))
}

/// Build a router whose verification-bundle route is backed by `trust` (other
/// ports unused).
fn trust_app(store: Store, trust: Arc<dyn axon_api::SenderTrustService>) -> axum::Router {
    let (live, _rx) = tokio::sync::broadcast::channel(16);
    axon_api::router(AppState::new(
        store,
        live,
        Arc::new(StubSender::ok("$unused:localhost")),
        Arc::new(StubLifecycle::ok(Uuid::nil())),
        Arc::new(StubVerification::ok("$unused-flow")),
        trust,
        Arc::new(StubDeviceList::ok()),
        Arc::new(StubTokenVerifier::ok()),
        Arc::new(StubMediaProxy),
        None,
    ))
}

/// Build a router whose device-list route is backed by `devices` (other ports
/// unused).
fn devices_app(store: Store, devices: Arc<dyn axon_api::DeviceListService>) -> axum::Router {
    let (live, _rx) = tokio::sync::broadcast::channel(16);
    axon_api::router(AppState::new(
        store,
        live,
        Arc::new(StubSender::ok("$unused:localhost")),
        Arc::new(StubLifecycle::ok(Uuid::nil())),
        Arc::new(StubVerification::ok("$unused-flow")),
        Arc::new(StubTrust::ok()),
        devices,
        Arc::new(StubTokenVerifier::ok()),
        Arc::new(StubMediaProxy),
        None,
    ))
}

/// Build a router whose media route is backed by `media` (other ports unused).
fn media_app(store: Store, media: Arc<dyn MediaProxy>) -> axum::Router {
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
        media,
        None,
    ))
}

/// `mimetype` is optional: pass `None` when the test doesn't care about
/// `Content-Type` derivation, or `Some(...)` (e.g. for the thumbnail route's
/// `Content-Type` assertions) to declare `info.mimetype`.
async fn insert_media_event(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    mxc_url: &str,
    mimetype: Option<&str>,
) -> String {
    let event_id = format!("$media-{}:localhost", Uuid::new_v4());
    let content = match mimetype {
        Some(mimetype) => json!({
            "msgtype": "m.image",
            "body": "image.png",
            "url": mxc_url,
            "info": { "mimetype": mimetype }
        }),
        None => json!({
            "msgtype": "m.image",
            "body": "image.png",
            "url": mxc_url
        }),
    };
    store
        .upsert_event(&NewEvent {
            event_id: &event_id,
            room_id,
            account_id,
            sender: "@alice:localhost",
            origin_ts: 1_700_000_000_000,
            event_type: "m.room.message",
            content: Some(content.clone()),
            raw_event: json!({ "type": "m.room.message", "content": content }),
            megolm_session_id: None,
            redacts: None,
            relates_to: None,
            decrypted_body_text: None,
        })
        .await
        .expect("insert media event");
    event_id
}

/// Shared body for `media_route_fails_closed_for_missing_and_redacted_metadata`
/// and `thumbnail_route_fails_closed_for_missing_and_redacted_metadata`: a
/// store miss, and a redacted event, must both `404` without the request ever
/// reaching the proxy. `path_suffix` distinguishes the full-media route
/// (`""`) from the thumbnail route (`"/thumbnail?width=64&height=64"`);
/// `calls_are_empty` reads whichever call log (`calls()`/`thumbnail_calls()`)
/// the route under test records to.
async fn assert_media_route_fails_closed(
    name_prefix: &str,
    path_suffix: &str,
    calls_are_empty: impl Fn(&ConfiguredMediaProxy) -> bool,
) {
    let store = store().await;
    let pool = store.pool().clone();
    let account_id = store
        .upsert_account(
            &format!("@{name_prefix}-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .expect("account")
        .account_id;
    let room_id = format!("!{name_prefix}-{}:localhost", Uuid::new_v4());
    let media = Arc::new(ConfiguredMediaProxy::ok(b"must not be returned"));
    let app = media_app(store.clone(), media.clone());

    let missing_mxc = format!("mxc://example.org/{}", Uuid::new_v4().simple());
    let missing_path = missing_mxc.trim_start_matches("mxc://");
    let (status, body) = get(
        &app,
        &format!("/v1/media/{account_id}/{missing_path}{path_suffix}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
    assert!(calls_are_empty(&media), "store miss must not reach proxy");

    let redacted_mxc = format!("mxc://example.org/{}", Uuid::new_v4().simple());
    let target = insert_media_event(&store, account_id, &room_id, &redacted_mxc, None).await;
    let redaction_id = format!("$redact-{}:localhost", Uuid::new_v4());
    store
        .upsert_event(&NewEvent {
            event_id: &redaction_id,
            room_id: &room_id,
            account_id,
            sender: "@alice:localhost",
            origin_ts: 1_700_000_000_001,
            event_type: "m.room.redaction",
            content: Some(json!({})),
            raw_event: json!({ "type": "m.room.redaction", "redacts": target }),
            megolm_session_id: None,
            redacts: Some(&target),
            relates_to: None,
            decrypted_body_text: None,
        })
        .await
        .expect("insert redaction");

    let redacted_path = redacted_mxc.trim_start_matches("mxc://");
    let (status, body) = get(
        &app,
        &format!("/v1/media/{account_id}/{redacted_path}{path_suffix}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
    assert!(
        calls_are_empty(&media),
        "redacted media must not reach proxy"
    );

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn media_route_fails_closed_for_missing_and_redacted_metadata() {
    assert_media_route_fails_closed("media-closed", "", |media| media.calls().is_empty()).await;
}

/// Shared body for `media_route_preserves_forbidden_and_not_connected_statuses`
/// and `thumbnail_route_preserves_forbidden_and_not_connected_statuses`:
/// `Forbidden`/`NotConnected` proxy outcomes must map to `403`/`503`
/// respectively. `path_suffix` distinguishes the full-media route from the
/// thumbnail route, as in [`assert_media_route_fails_closed`].
async fn assert_media_route_preserves_error_statuses(name_prefix: &str, path_suffix: &str) {
    let store = store().await;
    let pool = store.pool().clone();
    let account_id = store
        .upsert_account(
            &format!("@{name_prefix}-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .expect("account")
        .account_id;
    let room_id = format!("!{name_prefix}-{}:localhost", Uuid::new_v4());
    let mxc_url = format!("mxc://example.org/{}", Uuid::new_v4().simple());
    insert_media_event(&store, account_id, &room_id, &mxc_url, None).await;
    let media_path = mxc_url.trim_start_matches("mxc://");
    let uri = format!("/v1/media/{account_id}/{media_path}{path_suffix}");

    let forbidden = Arc::new(ConfiguredMediaProxy::failing(MediaOutcome::Forbidden(
        "media forbidden".to_owned(),
    )));
    let (status, body) = get(&media_app(store.clone(), forbidden), &uri).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "forbidden");

    let not_connected = Arc::new(ConfiguredMediaProxy::failing(MediaOutcome::NotConnected(
        "account unavailable".to_owned(),
    )));
    let (status, body) = get(&media_app(store.clone(), not_connected), &uri).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "service_unavailable");

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn media_route_preserves_forbidden_and_not_connected_statuses() {
    assert_media_route_preserves_error_statuses("media-errors", "").await;
}

/// Insert an event whose primary attachment is *encrypted* (`content.file`
/// carries the MXC url rather than the plain `content.url`), so
/// `encrypted_file_for_mxc` matches and the thumbnail route's `400` pre-check
/// fires.
async fn insert_encrypted_media_event(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    mxc_url: &str,
) -> String {
    let event_id = format!("$media-enc-{}:localhost", Uuid::new_v4());
    let content = json!({
        "msgtype": "m.image",
        "body": "image.png",
        "file": { "url": mxc_url, "key": {}, "iv": "", "hashes": {}, "v": "v2" },
        "info": { "mimetype": "image/png" }
    });
    store
        .upsert_event(&NewEvent {
            event_id: &event_id,
            room_id,
            account_id,
            sender: "@alice:localhost",
            origin_ts: 1_700_000_000_000,
            event_type: "m.room.message",
            content: Some(content.clone()),
            raw_event: json!({ "type": "m.room.message", "content": content }),
            megolm_session_id: None,
            redacts: None,
            relates_to: None,
            decrypted_body_text: None,
        })
        .await
        .expect("insert encrypted media event");
    event_id
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn thumbnail_route_rejects_encrypted_media_before_reaching_proxy() {
    let store = store().await;
    let pool = store.pool().clone();
    let account_id = store
        .upsert_account(
            &format!("@thumb-enc-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .expect("account")
        .account_id;
    let room_id = format!("!thumb-enc-{}:localhost", Uuid::new_v4());
    let mxc_url = format!("mxc://example.org/{}", Uuid::new_v4().simple());
    insert_encrypted_media_event(&store, account_id, &room_id, &mxc_url).await;
    let media = Arc::new(ConfiguredMediaProxy::ok(b"must not be returned"));
    let app = media_app(store.clone(), media.clone());

    let media_path = mxc_url.trim_start_matches("mxc://");
    let (status, body) = get(
        &app,
        &format!("/v1/media/{account_id}/{media_path}/thumbnail?width=64&height=64"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request");
    assert!(
        media.thumbnail_calls().is_empty(),
        "encrypted media must never reach the proxy's thumbnail method"
    );

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn thumbnail_route_serves_bytes_and_sets_headers() {
    let store = store().await;
    let pool = store.pool().clone();
    let account_id = store
        .upsert_account(
            &format!("@thumb-ok-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .expect("account")
        .account_id;
    let room_id = format!("!thumb-ok-{}:localhost", Uuid::new_v4());
    let mxc_url = format!("mxc://example.org/{}", Uuid::new_v4().simple());
    insert_media_event(&store, account_id, &room_id, &mxc_url, Some("image/png")).await;
    let media = Arc::new(ConfiguredMediaProxy::ok(b"thumbnail bytes"));
    let app = media_app(store.clone(), media.clone());

    let media_path = mxc_url.trim_start_matches("mxc://");
    let (status, headers, bytes) = get_media_bytes(
        &app,
        &format!("/v1/media/{account_id}/{media_path}/thumbnail?width=64&height=64&method=crop"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"thumbnail bytes");
    assert_eq!(headers["content-type"], "image/png");

    let calls = media.thumbnail_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].mxc_url, mxc_url);
    // The requested 64x64 snaps up to the next standard bucket (96) — see
    // `routes::media::snap_thumbnail_dimension`.
    assert_eq!(calls[0].spec.width, 96);
    assert_eq!(calls[0].spec.height, 96);
    assert_eq!(
        calls[0].spec.method,
        axon_core::media::ThumbnailMethod::Crop
    );

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn thumbnail_route_conditional_get_short_circuits_before_fetch() {
    let store = store().await;
    let pool = store.pool().clone();
    let account_id = store
        .upsert_account(
            &format!("@thumb-304-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .expect("account")
        .account_id;
    let room_id = format!("!thumb-304-{}:localhost", Uuid::new_v4());
    let mxc_url = format!("mxc://example.org/{}", Uuid::new_v4().simple());
    insert_media_event(&store, account_id, &room_id, &mxc_url, None).await;
    let media = Arc::new(ConfiguredMediaProxy::ok(b"thumbnail bytes"));
    let app = media_app(store.clone(), media.clone());

    // The requested 64x64 snaps up to the next standard bucket (96) — see
    // `routes::media::snap_thumbnail_dimension` — so the etag must be
    // computed against the *snapped* spec to match what the handler resolves.
    let spec = axon_core::media::ThumbnailSpec {
        width: 96,
        height: 96,
        method: axon_core::media::ThumbnailMethod::Scale,
    };
    let etag = format!("\"{}\"", media.etag_thumbnail(&mxc_url, spec));

    let media_path = mxc_url.trim_start_matches("mxc://");
    let (status, _headers, bytes) = get_media_bytes(
        &app,
        &format!("/v1/media/{account_id}/{media_path}/thumbnail?width=64&height=64"),
        Some(&etag),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(bytes.is_empty());
    assert!(
        media.thumbnail_calls().is_empty(),
        "a matching If-None-Match must short-circuit before calling the proxy"
    );

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn thumbnail_route_fails_closed_for_missing_and_redacted_metadata() {
    assert_media_route_fails_closed("thumb-closed", "/thumbnail?width=64&height=64", |media| {
        media.thumbnail_calls().is_empty()
    })
    .await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn thumbnail_route_preserves_forbidden_and_not_connected_statuses() {
    assert_media_route_preserves_error_statuses("thumb-errors", "/thumbnail?width=64&height=64")
        .await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn read_api_end_to_end() {
    let store = store().await;
    let pool = store.pool().clone();
    let account_user_id = format!("@http-{}:localhost", Uuid::new_v4());
    let account_id = store
        .upsert_account(&account_user_id, "https://hs.example.org")
        .await
        .expect("account")
        .account_id;
    let room_id = format!("!http-{}:localhost", Uuid::new_v4());

    // Two messages; a name so the summary is populated.
    let e1 = insert_message(&store, account_id, &room_id, 1_000, "first").await;
    let e2 = insert_message(&store, account_id, &room_id, 2_000, "second").await;
    // A state event, used below to assert `state_key` is exposed on reads. Its
    // ts sits *below* both messages so it doesn't interleave with the message
    // pagination assertions — `room_timeline` includes state events, so a member
    // event between e1 and e2 would land in the limit-1 page 2 instead of e1.
    let member_event_id = format!("$member-{}:localhost", Uuid::new_v4());
    let member_content = json!({ "membership": "join", "displayname": "Alice" });
    // `unsigned.prev_content` here models a displayname change: `membership`
    // stays "join" in both old and new content, which is exactly the case
    // issue #31 needs `prev_content` to disambiguate from a real join.
    let member_prev_content = json!({ "membership": "join", "displayname": "Alice Prior" });
    store
        .upsert_event(&NewEvent {
            event_id: &member_event_id,
            room_id: &room_id,
            account_id,
            sender: "@jamie:localhost",
            origin_ts: 500,
            event_type: "m.room.member",
            content: Some(member_content.clone()),
            raw_event: json!({
                "type": "m.room.member",
                "state_key": "@alice:localhost",
                "content": member_content,
                "unsigned": { "prev_content": member_prev_content }
            }),
            megolm_session_id: None,
            redacts: None,
            relates_to: None,
            decrypted_body_text: None,
        })
        .await
        .expect("insert membership event");
    // `upsert_event` alone only lands the event in the timeline; the
    // `/members` endpoint reads the resolved `room_state` projection, which
    // needs its own write.
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id: &room_id,
            event_type: "m.room.member",
            state_key: "@alice:localhost",
            event_id: &member_event_id,
            sender: "@jamie:localhost",
            origin_ts: 500,
            content: Some(json!({ "membership": "join", "displayname": "Alice" })),
        })
        .await
        .expect("member state");
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id: &room_id,
            event_type: "m.room.name",
            state_key: "",
            event_id: "$name:localhost",
            sender: "@alice:localhost",
            origin_ts: 1_500,
            content: Some(json!({ "name": "HTTP Room" })),
        })
        .await
        .expect("name");
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id: &room_id,
            event_type: "m.room.create",
            state_key: "",
            event_id: "$create:localhost",
            sender: "@alice:localhost",
            origin_ts: 1_600,
            content: Some(json!({ "type": "m.space" })),
        })
        .await
        .expect("create state");
    store
        .upsert_room_unread_counts(account_id, &room_id, 3, 1)
        .await
        .expect("seed unread counts");

    // The read endpoints don't touch the live-event bus or the message sender;
    // throwaway instances satisfy `AppState`.
    let (live, _rx) = tokio::sync::broadcast::channel(16);
    let member_profiles = Arc::new(StubMemberProfiles::new(vec![axon_api::MemberProfile {
        user_id: "@alice:localhost".to_owned(),
        avatar_url: Some("mxc://hs/profile-alice".to_owned()),
    }]));
    let app = axon_api::router(
        AppState::new(
            store.clone(),
            live,
            Arc::new(StubSender::ok("$unused:localhost")),
            Arc::new(StubLifecycle::ok(Uuid::nil())),
            Arc::new(StubVerification::ok("$unused-flow")),
            Arc::new(StubTrust::ok()),
            Arc::new(StubDeviceList::ok()),
            Arc::new(StubTokenVerifier::ok()),
            Arc::new(StubMediaProxy),
            None,
        )
        .with_member_profiles(member_profiles.clone()),
    );

    // GET /v1/rooms?account_id= — our room is present with its name + latest event.
    let (status, body) = get(&app, &format!("/v1/rooms?account_id={account_id}")).await;
    assert_eq!(status, StatusCode::OK);
    let rooms = body["data"].as_array().expect("data array");
    let room = rooms
        .iter()
        .find(|r| r["room_id"] == room_id.as_str())
        .expect("our room present");
    assert_eq!(room["name"], "HTTP Room");
    assert_eq!(room["last_activity_ts"], 2_000);
    assert_eq!(room["last_event_id"], e2.as_str());
    assert_eq!(room["account_id"], account_id.to_string());
    assert_eq!(room["account_user_id"], account_user_id);
    assert_eq!(room["room_type"], "m.space");
    assert_eq!(room["notification_count"], 3);
    assert_eq!(room["highlight_count"], 1);

    // Timeline, page 1 (newest): limit 1 -> [e2] with a next_cursor.
    let base = format!("/v1/accounts/{account_id}/rooms/{room_id}/timeline");
    let (status, page1) = get(&app, &format!("{base}?limit=1")).await;
    assert_eq!(status, StatusCode::OK);
    let evs = page1["data"]["events"].as_array().expect("events");
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0]["event_id"], e2.as_str());
    assert_eq!(evs[0]["type"], "m.room.message");
    assert_eq!(evs[0]["body"], "second");
    let cursor = page1["data"]["next_cursor"].as_str().expect("next_cursor");

    // Page 2 via the cursor: [e1], and no overlap with page 1.
    let (status, page2) = get(&app, &format!("{base}?limit=1&cursor={cursor}")).await;
    assert_eq!(status, StatusCode::OK);
    let evs2 = page2["data"]["events"].as_array().expect("events");
    assert_eq!(evs2.len(), 1);
    assert_eq!(evs2[0]["event_id"], e1.as_str());

    // A malformed cursor is a 400 with a bad_request code.
    let (status, err) = get(&app, &format!("{base}?cursor=not-a-cursor")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["code"], "bad_request");

    // cursor and at_ts are mutually exclusive: supplying both is a 400, not a
    // silent preference for one over the other.
    let (status, err) = get(&app, &format!("{base}?cursor={cursor}&at_ts=0")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["code"], "bad_request");

    // A non-UUID account_id is a 400 in the *envelope* (not axum's plain-text
    // rejection): once in a query filter, once in a path segment.
    let (status, err) = get(&app, "/v1/rooms?account_id=12345").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["code"], "bad_request");

    let (status, err) = get(&app, "/v1/accounts/12345/events/$x:localhost").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["code"], "bad_request");

    // GET single event -> 200 with content.
    let (status, ev) = get(&app, &format!("/v1/accounts/{account_id}/events/{e1}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ev["data"]["event_id"], e1.as_str());
    assert_eq!(ev["data"]["body"], "first");
    assert_eq!(ev["data"]["state_key"], Value::Null);
    assert_eq!(ev["data"]["prev_content"], Value::Null);
    assert_eq!(ev["data"]["redacted"], false);

    let (status, member) = get(
        &app,
        &format!("/v1/accounts/{account_id}/events/{member_event_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(member["data"]["state_key"], "@alice:localhost");
    assert_eq!(member["data"]["sender"], "@jamie:localhost");
    assert_eq!(member["data"]["prev_content"], member_prev_content);

    let (status, members) = get(
        &app,
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/members"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let members = members["data"].as_array().expect("members");
    assert_eq!(members.len(), 1);
    assert_eq!(members[0]["user_id"], "@alice:localhost");
    assert_eq!(members[0]["avatar_url"], "mxc://hs/profile-alice");
    assert_eq!(
        member_profiles.calls(),
        vec![(room_id.clone(), vec!["@alice:localhost".to_owned()])]
    );

    // Unknown event -> 404 with not_found code.
    let (status, err) = get(
        &app,
        &format!("/v1/accounts/{account_id}/events/$nope:localhost"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(err["error"]["code"], "not_found");

    // Clean up: delete the account, cascading to its events + room state, so the
    // seeded test rows don't leak into a real `/v1/rooms` against the same DB.
    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

/// The five room-state read endpoints added for issue #404 / ADR 0084: spaces
/// hierarchy (both directions), pinned messages, room info, and the upgrade
/// chain. Uses the plain [`read_app`] (no member-profile enrichment needed
/// here).
#[tokio::test]
#[ignore = "requires Postgres"]
async fn room_state_read_endpoints() {
    let store = store().await;
    let pool = store.pool().clone();
    let account_id = store
        .upsert_account(
            &format!("@rsr-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .expect("account")
        .account_id;

    let space_id = format!("!space-{}:localhost", Uuid::new_v4());
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());
    let parent_id = format!("!parent-{}:localhost", Uuid::new_v4());
    let successor_id = format!("!successor-{}:localhost", Uuid::new_v4());
    let predecessor_id = format!("!predecessor-{}:localhost", Uuid::new_v4());

    // `room_id` is a space-child of `space_id`, and has its own name/avatar so
    // the children read's enrichment has something to find.
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id: &space_id,
            event_type: "m.space.child",
            state_key: &room_id,
            event_id: "$child:localhost",
            sender: "@alice:localhost",
            origin_ts: 1_000,
            content: Some(json!({ "via": ["hs.example.org"], "order": "a", "suggested": true })),
        })
        .await
        .expect("space child state");
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id: &room_id,
            event_type: "m.room.name",
            state_key: "",
            event_id: "$name:localhost",
            sender: "@alice:localhost",
            origin_ts: 1,
            content: Some(json!({ "name": "Main Room" })),
        })
        .await
        .expect("name state");

    // `room_id`'s parent space, likewise named so the parents read's
    // enrichment has something to find.
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id: &room_id,
            event_type: "m.space.parent",
            state_key: &parent_id,
            event_id: "$parent:localhost",
            sender: "@alice:localhost",
            origin_ts: 1,
            content: Some(json!({ "via": ["hs.example.org"], "canonical": true })),
        })
        .await
        .expect("space parent state");
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id: &parent_id,
            event_type: "m.room.name",
            state_key: "",
            event_id: "$parent-name:localhost",
            sender: "@alice:localhost",
            origin_ts: 1,
            content: Some(json!({ "name": "Parent Space" })),
        })
        .await
        .expect("parent name state");

    // Two messages, pinned newest-first — the reverse of send order, so the
    // ordering assertion below can't pass by accident.
    let e1 = insert_message(&store, account_id, &room_id, 1_000, "first").await;
    let e2 = insert_message(&store, account_id, &room_id, 2_000, "second").await;
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id: &room_id,
            event_type: "m.room.pinned_events",
            state_key: "",
            event_id: "$pinned:localhost",
            sender: "@alice:localhost",
            origin_ts: 1,
            content: Some(json!({ "pinned": [e2, e1] })),
        })
        .await
        .expect("pinned state");

    // The four room-info singletons.
    for (event_type, content) in [
        ("m.room.join_rules", json!({ "join_rule": "invite" })),
        (
            "m.room.history_visibility",
            json!({ "history_visibility": "shared" }),
        ),
        (
            "m.room.guest_access",
            json!({ "guest_access": "forbidden" }),
        ),
        (
            "m.room.encryption",
            json!({ "algorithm": "m.megolm.v1.aes-sha2" }),
        ),
    ] {
        store
            .upsert_room_state(&RoomStateUpsert {
                account_id,
                room_id: &room_id,
                event_type,
                state_key: "",
                event_id: &format!("$info-{}:localhost", Uuid::new_v4()),
                sender: "@alice:localhost",
                origin_ts: 1,
                content: Some(content),
            })
            .await
            .expect("room info state");
    }

    // Tombstoned to `successor_id`, and itself an upgrade of `predecessor_id`.
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id: &room_id,
            event_type: "m.room.tombstone",
            state_key: "",
            event_id: "$tombstone:localhost",
            sender: "@alice:localhost",
            origin_ts: 1,
            content: Some(json!({ "body": "upgraded", "replacement_room": successor_id })),
        })
        .await
        .expect("tombstone state");
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id: &room_id,
            event_type: "m.room.create",
            state_key: "",
            event_id: "$create:localhost",
            sender: "@alice:localhost",
            origin_ts: 1,
            content: Some(json!({ "predecessor": { "room_id": predecessor_id } })),
        })
        .await
        .expect("create state");

    let app = read_app(store.clone());

    // GET .../space/children on the space -> one enriched child.
    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/rooms/{space_id}/space/children"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let children = body["data"].as_array().expect("children array");
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["room_id"], room_id.as_str());
    assert_eq!(children[0]["order"], "a");
    assert_eq!(children[0]["suggested"], true);
    assert_eq!(children[0]["name"], "Main Room");

    // GET .../space/parents on the room -> one enriched parent.
    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/space/parents"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parents = body["data"].as_array().expect("parents array");
    assert_eq!(parents.len(), 1);
    assert_eq!(parents[0]["room_id"], parent_id.as_str());
    assert_eq!(parents[0]["canonical"], true);
    assert_eq!(parents[0]["name"], "Parent Space");

    // GET .../pinned -> [e2, e1], pinned-list order, hydrated content.
    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/pinned"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let pinned = body["data"].as_array().expect("pinned array");
    assert_eq!(pinned.len(), 2);
    assert_eq!(pinned[0]["event_id"], e2.as_str());
    assert_eq!(pinned[0]["body"], "second");
    assert_eq!(pinned[1]["event_id"], e1.as_str());
    assert_eq!(pinned[1]["body"], "first");

    // GET .../info -> the four bundled singletons.
    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/info"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["join_rule"], "invite");
    assert_eq!(body["data"]["history_visibility"], "shared");
    assert_eq!(body["data"]["guest_access"], "forbidden");
    assert_eq!(body["data"]["encryption_algorithm"], "m.megolm.v1.aes-sha2");

    // GET .../upgrade -> both directions.
    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/upgrade"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["tombstoned_to"], successor_id.as_str());
    assert_eq!(body["data"]["upgraded_from"], predecessor_id.as_str());

    // An unknown room reads as empty/all-null on every one of the five, not a 404.
    let unknown_room = format!("!unknown-{}:localhost", Uuid::new_v4());
    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/rooms/{unknown_room}/space/children"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["data"].as_array().expect("empty children").is_empty());

    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/rooms/{unknown_room}/space/parents"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["data"].as_array().expect("empty parents").is_empty());

    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/rooms/{unknown_room}/pinned"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["data"].as_array().expect("empty pinned").is_empty());

    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/rooms/{unknown_room}/info"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["join_rule"], Value::Null);
    assert_eq!(body["data"]["encryption_algorithm"], Value::Null);

    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/rooms/{unknown_room}/upgrade"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["tombstoned_to"], Value::Null);
    assert_eq!(body["data"]["upgraded_from"], Value::Null);

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn accounts_read_api() {
    let store = store().await;
    let pool = store.pool().clone();

    // Three accounts in distinct lifecycle states. Unique user ids per run so the
    // assertions hold regardless of whatever else is in the shared test DB.
    let hs = "https://hs.example.org";
    let active_user = format!("@active-{}:localhost", Uuid::new_v4());
    let deactivated_user = format!("@deactivated-{}:localhost", Uuid::new_v4());
    let deleting_user = format!("@deleting-{}:localhost", Uuid::new_v4());

    let active = store
        .upsert_account(&active_user, hs)
        .await
        .expect("active");
    let deactivated = store
        .upsert_account(&deactivated_user, hs)
        .await
        .expect("deactivated");
    let deleting = store
        .upsert_account(&deleting_user, hs)
        .await
        .expect("deleting");
    store
        .set_account_state(deactivated.account_id, AccountState::Deactivated)
        .await
        .expect("deactivate");
    store
        .set_account_state(deleting.account_id, AccountState::Deleting)
        .await
        .expect("set deleting");

    let (live, _rx) = tokio::sync::broadcast::channel(16);
    let app = axon_api::router(AppState::new(
        store.clone(),
        live,
        Arc::new(StubSender::ok("$unused:localhost")),
        Arc::new(StubLifecycle::ok(Uuid::nil())),
        Arc::new(StubVerification::ok("$unused-flow")),
        Arc::new(StubTrust::ok()),
        Arc::new(StubDeviceList::ok()),
        Arc::new(StubTokenVerifier::ok()),
        Arc::new(StubMediaProxy),
        None,
    ));

    // GET /v1/accounts — the client-visible set: `active` and `deactivated` are
    // both listed (so a logged-out account is discoverable for re-login), but the
    // transient `deleting` teardown state is excluded.
    let (status, body) = get(&app, "/v1/accounts").await;
    assert_eq!(status, StatusCode::OK);
    let rows = body["data"].as_array().expect("data array");
    let find = |id: Uuid| rows.iter().find(|a| a["account_id"] == id.to_string());

    let active_row = find(active.account_id).expect("active listed");
    assert_eq!(active_row["state"], "active");
    // `verified` now surfaces the persisted column (ADR 0026): a freshly upserted
    // account is unverified until a recover/verify derives otherwise, so it reads
    // a concrete `false` rather than the old always-`null` stub.
    assert_eq!(active_row["verified"], false);
    assert_eq!(active_row["user_id"], active_user);
    assert_eq!(active_row["homeserver_url"], hs);
    // No `SyncStateProvider` was injected (this router uses the plain `new`
    // constructor), so every account reports the default (ADR 0030, issue #241).
    assert_eq!(active_row["sync_state"], "connecting");
    // The token is never exposed, under any key.
    assert!(active_row.get("access_token").is_none());
    assert!(active_row.get("access_token_encrypted").is_none());

    let deactivated_row = find(deactivated.account_id).expect("deactivated listed");
    assert_eq!(deactivated_row["state"], "deactivated");

    assert!(
        find(deleting.account_id).is_none(),
        "deleting account must not appear in the list"
    );

    // GET /v1/accounts/{id} — 200 for a known account, in whatever state. The
    // list omits `deleting`, but a by-id read is not filtered at all.
    let (status, one) = get(&app, &format!("/v1/accounts/{}", active.account_id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["data"]["account_id"], active.account_id.to_string());
    assert_eq!(one["data"]["state"], "active");

    let (status, deactivated_by_id) =
        get(&app, &format!("/v1/accounts/{}", deactivated.account_id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deactivated_by_id["data"]["state"], "deactivated");

    // The `deleting` account is absent from the list but still readable by id —
    // the by-id read is unfiltered.
    let (status, deleting_by_id) =
        get(&app, &format!("/v1/accounts/{}", deleting.account_id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleting_by_id["data"]["state"], "deleting");

    // Unknown account id -> 404 with not_found code.
    let (status, err) = get(&app, &format!("/v1/accounts/{}", Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(err["error"]["code"], "not_found");

    // A non-UUID id -> 400 in the envelope (not axum's plain-text rejection).
    let (status, err) = get(&app, "/v1/accounts/not-a-uuid").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["code"], "bad_request");

    for id in [
        active.account_id,
        deactivated.account_id,
        deleting.account_id,
    ] {
        sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect("cleanup");
    }
}

/// `AccountDto.sync_state` (ADR 0030, issue #241) reflects whatever the
/// injected `SyncStateProvider` reports, on every route that returns an
/// account — list, by-id, login, and logout.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn accounts_sync_state_reflects_the_provider() {
    let store = store().await;
    let pool = store.pool().clone();

    let user_id = format!("@sync-state-{}:localhost", Uuid::new_v4());
    let account = store
        .upsert_account(&user_id, "https://hs.example.org")
        .await
        .expect("upsert");

    let (live, _rx) = tokio::sync::broadcast::channel(16);
    let app = axon_api::router(
        AppState::new(
            store.clone(),
            live,
            Arc::new(StubSender::ok("$unused:localhost")),
            Arc::new(StubLifecycle::ok(account.account_id)),
            Arc::new(StubVerification::ok("$unused-flow")),
            Arc::new(StubTrust::ok()),
            Arc::new(StubDeviceList::ok()),
            Arc::new(StubTokenVerifier::ok()),
            Arc::new(StubMediaProxy),
            None,
        )
        .with_sync_state(Arc::new(StubSyncState("ready"))),
    );

    let (status, one) = get(&app, &format!("/v1/accounts/{}", account.account_id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["data"]["sync_state"], "ready");

    let (status, list) = get(&app, "/v1/accounts").await;
    assert_eq!(status, StatusCode::OK);
    let rows = list["data"].as_array().expect("data array");
    let row = rows
        .iter()
        .find(|a| a["account_id"] == account.account_id.to_string())
        .expect("account listed");
    assert_eq!(row["sync_state"], "ready");

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account.account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

// ---- Bearer-token auth gate (M7b) ----
//
// The gate is a single middleware layer over every `/v1/` route, so one route
// (login, backed by a stub that records calls) is representative: a missing or
// wrong token is rejected *before* the handler runs, and a read route is gated
// the same way.

#[tokio::test]
#[ignore = "requires Postgres"]
async fn auth_gate_rejects_missing_and_invalid_tokens_before_the_handler() {
    let store = store().await;
    let stub = Arc::new(StubLifecycle::ok(Uuid::nil()));
    let app = lifecycle_app(store, stub.clone());
    let body = json!({
        "homeserver_url": "https://hs.example.org",
        "username": "@a:localhost",
        "password": "pw",
    });

    // No Authorization header → 401, and the lifecycle port is never invoked. A
    // missing credential gets the bare RFC 6750 `Bearer` challenge.
    let (status, headers, err) =
        request_parts(&app, "POST", "/v1/accounts/login", Some(body.clone()), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(err["error"]["code"], "unauthorized");
    assert_eq!(headers["www-authenticate"], "Bearer");

    // A wrong token → 401, still short-circuited before the handler. A present
    // but rejected token gets the `error="invalid_token"` challenge (§3.1).
    let (status, headers, err) = request_parts(
        &app,
        "POST",
        "/v1/accounts/login",
        Some(body),
        Some("Bearer not-the-test-token"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(err["error"]["code"], "unauthorized");
    assert_eq!(
        headers["www-authenticate"],
        "Bearer error=\"invalid_token\""
    );

    assert!(
        stub.calls().is_empty(),
        "the auth gate must short-circuit before the lifecycle port"
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn auth_gate_covers_read_routes_but_healthz_is_open() {
    let store = store().await;
    let app = lifecycle_app(store, Arc::new(StubLifecycle::ok(Uuid::nil())));

    // A plain read route is gated too: no token → 401 carrying the bearer challenge.
    let (status, headers, err) = request_parts(&app, "GET", "/v1/accounts", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(err["error"]["code"], "unauthorized");
    assert_eq!(headers["www-authenticate"], "Bearer");

    // The unversioned liveness probe carries no auth, so a monitor can reach it.
    let (status, _) = request(&app, "GET", "/healthz", None, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn browser_fallback_serves_html_for_root_and_non_api_misses() {
    let store = store().await;
    let app = read_app(store);

    for uri in ["/", "/not-a-route"] {
        let (status, headers, body) = request_text_parts(&app, "GET", uri, None).await;
        assert_eq!(status, StatusCode::OK, "uri: {uri}");
        assert_eq!(headers["content-type"], "text/html; charset=utf-8");
        assert!(body.contains("Axon"), "uri: {uri}");
        assert!(
            body.contains("self-hosted Matrix agent and API server"),
            "uri: {uri}"
        );
        assert!(
            body.contains("No web interface is served at this address"),
            "uri: {uri}"
        );
        assert!(body.contains("axon-tui"), "uri: {uri}");
        assert!(body.contains("/healthz"), "uri: {uri}");
        assert!(body.contains("/v1/"), "uri: {uri}");
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn unknown_v1_paths_stay_in_the_api_boundary() {
    let store = store().await;
    let app = read_app(store);

    let (status, headers, err) = request_parts(&app, "GET", "/v1/not-a-route", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(err["error"]["code"], "unauthorized");
    assert_eq!(headers["www-authenticate"], "Bearer");

    let (status, body) = request(&app, "GET", "/v1/not-a-route", None, Some(&bearer())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
    assert_eq!(body["error"]["message"], "route not found");
}

// ---- Lifecycle: login / logout / delete / recover ----

#[tokio::test]
#[ignore = "requires Postgres"]
async fn login_succeeds_routes_to_port_and_envelopes_account() {
    let store = store().await;
    let pool = store.pool().clone();

    // Seed the account the stub will "log in" — the handler reads it back by id to
    // build the response, so it must exist. Unique id keeps the shared DB clean.
    let hs = "https://hs.example.org";
    let user = format!("@login-{}:localhost", Uuid::new_v4());
    let account = store.upsert_account(&user, hs).await.expect("seed");

    let stub = Arc::new(StubLifecycle::ok(account.account_id));
    let app = lifecycle_app(store.clone(), stub.clone());

    let (status, body) = request(
        &app,
        "POST",
        "/v1/accounts/login",
        Some(json!({ "homeserver_url": hs, "username": user, "password": "hunter2" })),
        Some(&bearer()),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["account_id"], account.account_id.to_string());
    assert_eq!(body["data"]["user_id"], user);
    // The password is never echoed back in the account view.
    assert!(body["data"].get("password").is_none());

    // The handler passed the decoded request straight through to the port.
    assert_eq!(
        stub.calls(),
        vec![LoginCall {
            homeserver_url: Some(hs.to_owned()),
            username: user.clone(),
            password: "hunter2".to_owned(),
        }]
    );

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account.account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn login_without_homeserver_url_forwards_none_for_discovery() {
    let store = store().await;
    let pool = store.pool().clone();

    let user = format!("@discover-{}:localhost", Uuid::new_v4());
    let account = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("seed");

    let stub = Arc::new(StubLifecycle::ok(account.account_id));
    let app = lifecycle_app(store.clone(), stub.clone());

    // No `homeserver_url` in the body: the handler must accept the request and
    // forward `None` so the lifecycle backend performs discovery.
    let (status, body) = request(
        &app,
        "POST",
        "/v1/accounts/login",
        Some(json!({ "username": user, "password": "hunter2" })),
        Some(&bearer()),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["account_id"], account.account_id.to_string());
    assert_eq!(
        stub.calls(),
        vec![LoginCall {
            homeserver_url: None,
            username: user.clone(),
            password: "hunter2".to_owned(),
        }]
    );

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account.account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn login_malformed_body_is_enveloped_400_and_skips_port() {
    let store = store().await;
    let stub = Arc::new(StubLifecycle::ok(Uuid::nil()));
    let app = lifecycle_app(store, stub.clone());

    // Missing the required `password` field → JSON decode failure, but only after
    // the auth gate has admitted the request.
    let (status, err) = request(
        &app,
        "POST",
        "/v1/accounts/login",
        Some(json!({ "homeserver_url": "https://hs.example.org", "username": "@a:localhost" })),
        Some(&bearer()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["code"], "bad_request");
    assert!(stub.calls().is_empty());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn login_error_maps_to_status() {
    let store = store().await;
    let body = json!({
        "homeserver_url": "https://hs.example.org",
        "username": "@a:localhost",
        "password": "pw",
    });

    let cases = [
        (
            LoginOutcome::InvalidRequest("bad".into()),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            LoginOutcome::AuthFailed("nope".into()),
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        ),
        (
            LoginOutcome::Conflict("already".into()),
            StatusCode::CONFLICT,
            "conflict",
        ),
        (
            LoginOutcome::Upstream("hs down".into()),
            StatusCode::BAD_GATEWAY,
            "bad_gateway",
        ),
        (
            LoginOutcome::Internal,
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
        ),
    ];

    for (outcome, want_status, want_code) in cases {
        let app = lifecycle_app(store.clone(), Arc::new(StubLifecycle::failing(outcome)));
        let (status, err) = request(
            &app,
            "POST",
            "/v1/accounts/login",
            Some(body.clone()),
            Some(&bearer()),
        )
        .await;
        assert_eq!(status, want_status);
        assert_eq!(err["error"]["code"], want_code);
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn import_token_succeeds_routes_to_port_and_envelopes_account() {
    let store = store().await;
    let pool = store.pool().clone();

    // Seed the account the stub will "import" — the handler reads it back by id
    // to build the response, so it must exist.
    let hs = "https://hs.example.org";
    let user = format!("@import-{}:localhost", Uuid::new_v4());
    let account = store.upsert_account(&user, hs).await.expect("seed");

    let stub = Arc::new(StubLifecycle::ok(account.account_id));
    let app = lifecycle_app(store.clone(), stub.clone());

    let (status, body) = request(
        &app,
        "POST",
        "/v1/accounts/import",
        Some(json!({
            "homeserver_url": hs,
            "username": user,
            "access_token": "syt_abc123",
            "device_id": "IMPORTEDDEV",
        })),
        Some(&bearer()),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["account_id"], account.account_id.to_string());
    assert_eq!(body["data"]["user_id"], user);
    // The token is never echoed back in the account view.
    assert!(body["data"].get("access_token").is_none());

    // The handler passed the decoded request straight through to the port.
    assert_eq!(
        stub.import_token_calls(),
        vec![ImportTokenCall {
            homeserver_url: hs.to_owned(),
            username: user.clone(),
            access_token: "syt_abc123".to_owned(),
            device_id: "IMPORTEDDEV".to_owned(),
        }]
    );
    // The regular login port is untouched by the import route.
    assert!(stub.calls().is_empty());

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account.account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn import_token_malformed_body_is_enveloped_400_and_skips_port() {
    let store = store().await;
    let stub = Arc::new(StubLifecycle::ok(Uuid::nil()));
    let app = lifecycle_app(store, stub.clone());

    // Missing the required `device_id` field → JSON decode failure, but only
    // after the auth gate has admitted the request.
    let (status, err) = request(
        &app,
        "POST",
        "/v1/accounts/import",
        Some(json!({
            "homeserver_url": "https://hs.example.org",
            "username": "@a:localhost",
            "access_token": "syt_abc123",
        })),
        Some(&bearer()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["code"], "bad_request");
    assert!(stub.import_token_calls().is_empty());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn import_token_error_maps_to_status() {
    let store = store().await;
    let body = json!({
        "homeserver_url": "https://hs.example.org",
        "username": "@a:localhost",
        "access_token": "syt_abc123",
        "device_id": "IMPORTEDDEV",
    });

    let cases = [
        (
            LoginOutcome::InvalidRequest("bad".into()),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            LoginOutcome::AuthFailed("token mismatch".into()),
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        ),
        (
            LoginOutcome::Conflict("already".into()),
            StatusCode::CONFLICT,
            "conflict",
        ),
        (
            LoginOutcome::Upstream("hs down".into()),
            StatusCode::BAD_GATEWAY,
            "bad_gateway",
        ),
        (
            LoginOutcome::Internal,
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
        ),
    ];

    for (outcome, want_status, want_code) in cases {
        let app = lifecycle_app(
            store.clone(),
            Arc::new(StubLifecycle::import_token_failing(outcome)),
        );
        let (status, err) = request(
            &app,
            "POST",
            "/v1/accounts/import",
            Some(body.clone()),
            Some(&bearer()),
        )
        .await;
        assert_eq!(status, want_status);
        assert_eq!(err["error"]["code"], want_code);
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn logout_succeeds_and_envelopes_deactivated_account() {
    let store = store().await;
    let pool = store.pool().clone();

    // Seed the account already `deactivated` to mirror the post-logout row the
    // handler reads back (the stubbed port doesn't touch the DB — the real
    // transition is covered by the axon-sync lifecycle tests).
    let hs = "https://hs.example.org";
    let user = format!("@logout-{}:localhost", Uuid::new_v4());
    let account = store.upsert_account(&user, hs).await.expect("seed");
    store
        .set_account_state(account.account_id, AccountState::Deactivated)
        .await
        .expect("deactivate");

    let stub = Arc::new(StubLifecycle::ok(account.account_id));
    let app = lifecycle_app(store.clone(), stub.clone());

    let (status, body) = request(
        &app,
        "POST",
        &format!("/v1/accounts/{}/logout", account.account_id),
        None,
        Some(&bearer()),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["account_id"], account.account_id.to_string());
    assert_eq!(body["data"]["state"], "deactivated");
    // The handler routed the id straight through to the port.
    assert_eq!(stub.logout_calls(), vec![account.account_id]);

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account.account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn logout_error_maps_to_status() {
    let store = store().await;
    let id = Uuid::new_v4();

    let cases = [
        (
            LogoutOutcome::NotFound("nope".into()),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            LogoutOutcome::Conflict("deleting".into()),
            StatusCode::CONFLICT,
            "conflict",
        ),
        (
            LogoutOutcome::Internal,
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
        ),
    ];

    for (outcome, want_status, want_code) in cases {
        let app = lifecycle_app(
            store.clone(),
            Arc::new(StubLifecycle::logout_failing(outcome)),
        );
        let (status, err) = request(
            &app,
            "POST",
            &format!("/v1/accounts/{id}/logout"),
            None,
            Some(&bearer()),
        )
        .await;
        assert_eq!(status, want_status);
        assert_eq!(err["error"]["code"], want_code);
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn delete_succeeds_with_204_and_routes_to_port() {
    let store = store().await;
    let stub = Arc::new(StubLifecycle::ok(Uuid::nil()));
    let app = lifecycle_app(store, stub.clone());

    let id = Uuid::new_v4();
    let (status, body) = request(
        &app,
        "DELETE",
        &format!("/v1/accounts/{id}"),
        None,
        Some(&bearer()),
    )
    .await;

    // 204 No Content — the resource is gone, nothing to return.
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(body, Value::Null);
    // The handler routed the id straight through to the port.
    assert_eq!(stub.delete_calls(), vec![id]);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn delete_error_maps_to_status() {
    let store = store().await;
    let id = Uuid::new_v4();

    let cases = [
        (
            DeleteOutcome::NotFound("nope".into()),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            DeleteOutcome::Conflict("draining".into()),
            StatusCode::CONFLICT,
            "conflict",
        ),
        (
            DeleteOutcome::Internal,
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
        ),
    ];

    for (outcome, want_status, want_code) in cases {
        let app = lifecycle_app(
            store.clone(),
            Arc::new(StubLifecycle::delete_failing(outcome)),
        );
        let (status, err) = request(
            &app,
            "DELETE",
            &format!("/v1/accounts/{id}"),
            None,
            Some(&bearer()),
        )
        .await;
        assert_eq!(status, want_status);
        assert_eq!(err["error"]["code"], want_code);
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn recover_succeeds_and_envelopes_account_with_verified() {
    let store = store().await;
    let pool = store.pool().clone();

    // Seed the account `active` (recover requires it) and mark it verified to
    // mirror the post-recover row the handler reads back — the stubbed port
    // doesn't touch the DB, so the real cross-signing + flag write are covered by
    // the axon-sync lifecycle tests; here we assert the read-back is enveloped.
    let hs = "https://hs.example.org";
    let user = format!("@recover-{}:localhost", Uuid::new_v4());
    let account = store.upsert_account(&user, hs).await.expect("seed");
    store
        .set_account_verified(account.account_id, true)
        .await
        .expect("verify");

    let stub = Arc::new(StubLifecycle::ok(account.account_id));
    let app = lifecycle_app(store.clone(), stub.clone());

    let (status, resp) = request(
        &app,
        "POST",
        &format!("/v1/accounts/{}/recover", account.account_id),
        Some(json!({ "recovery_key": "EsTc SomeRecoveryKey" })),
        Some(&bearer()),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["data"]["account_id"], account.account_id.to_string());
    assert_eq!(resp["data"]["verified"], true);
    // The handler forwarded the id + recovery key straight to the port.
    assert_eq!(
        stub.recover_calls(),
        vec![(account.account_id, "EsTc SomeRecoveryKey".to_string())]
    );

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account.account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn recover_malformed_body_is_enveloped_400_and_skips_port() {
    let store = store().await;
    let stub = Arc::new(StubLifecycle::ok(Uuid::nil()));
    let app = lifecycle_app(store, stub.clone());

    // A valid token so the request reaches body decoding (missing `recovery_key`),
    // proving the 400 is decode-side, not the auth gate.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/v1/accounts/{}/recover", Uuid::new_v4()),
        Some(json!({})),
        Some(&bearer()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(stub.recover_calls().is_empty());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn recover_error_maps_to_status() {
    let store = store().await;
    let id = Uuid::new_v4();
    let body = json!({ "recovery_key": "k" });

    let cases = [
        (
            RecoverOutcome::NotFound("nope".into()),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            RecoverOutcome::Conflict("not active".into()),
            StatusCode::CONFLICT,
            "conflict",
        ),
        (
            RecoverOutcome::BadRequest("bad key".into()),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            RecoverOutcome::Internal,
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
        ),
    ];

    for (outcome, want_status, want_code) in cases {
        let app = lifecycle_app(
            store.clone(),
            Arc::new(StubLifecycle::recover_failing(outcome)),
        );
        let (status, err) = request(
            &app,
            "POST",
            &format!("/v1/accounts/{id}/recover"),
            Some(body.clone()),
            Some(&bearer()),
        )
        .await;
        assert_eq!(status, want_status);
        assert_eq!(err["error"]["code"], want_code);
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn redecrypt_utds_succeeds_and_envelopes_counts() {
    let store = store().await;
    let id = Uuid::new_v4();
    let stats = RedecryptUtdsStats {
        selected: 10,
        attempted: 8,
        decrypted: 3,
        still_pending: 7,
        timed_out: false,
    };
    let stub = Arc::new(StubLifecycle::redecrypt_failing(RedecryptOutcome::Ok(
        stats,
    )));
    let app = lifecycle_app(store, stub.clone());

    let (status, resp) = request(
        &app,
        "POST",
        &format!("/v1/accounts/{id}/utds/redecrypt"),
        None,
        Some(&bearer()),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["data"]["selected"], 10);
    assert_eq!(resp["data"]["attempted"], 8);
    assert_eq!(resp["data"]["decrypted"], 3);
    assert_eq!(resp["data"]["still_pending"], 7);
    assert_eq!(resp["data"]["timed_out"], false);
    assert_eq!(stub.redecrypt_calls(), vec![id]);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn redecrypt_utds_error_maps_to_status() {
    let store = store().await;
    let id = Uuid::new_v4();

    let cases = [
        (
            RedecryptOutcome::NotFound("nope".into()),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            RedecryptOutcome::Conflict("not active".into()),
            StatusCode::CONFLICT,
            "conflict",
        ),
        (
            RedecryptOutcome::Internal,
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
        ),
    ];

    for (outcome, want_status, want_code) in cases {
        let app = lifecycle_app(
            store.clone(),
            Arc::new(StubLifecycle::redecrypt_failing(outcome)),
        );
        let (status, err) = request(
            &app,
            "POST",
            &format!("/v1/accounts/{id}/utds/redecrypt"),
            None,
            Some(&bearer()),
        )
        .await;
        assert_eq!(status, want_status);
        assert_eq!(err["error"]["code"], want_code);
    }
}

// ---- Interactive SAS verification (M7a PR6) ----

#[tokio::test]
#[ignore = "requires Postgres"]
async fn verify_start_succeeds_and_returns_flow_id() {
    let store = store().await;
    let verify = Arc::new(StubVerification::ok("$flow-abc"));
    let app = verify_app(store, verify.clone());

    let account_id = Uuid::new_v4();
    let (status, body) = request(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/verify"),
        Some(json!({ "device_id": "TRUSTEDDEV" })),
        Some(&bearer()),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["flow_id"], "$flow-abc");
    // The handler forwarded the id + device straight to the port.
    assert_eq!(
        verify.calls(),
        vec![VerifyCall::Start {
            account_id,
            user_id: None,
            device_id: Some("TRUSTEDDEV".to_owned()),
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn verify_start_rejects_ambiguous_target_before_service() {
    let store = store().await;
    let verify = Arc::new(StubVerification::ok("$flow-unused"));
    let app = verify_app(store, verify.clone());

    let account_id = Uuid::new_v4();
    let (status, body) = request(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/verify"),
        Some(json!({ "user_id": "@peer:localhost", "device_id": "TRUSTEDDEV" })),
        Some(&bearer()),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("exactly one verification target"));
    assert!(verify.calls().is_empty());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn verify_start_rejects_missing_target_before_service() {
    let store = store().await;
    let verify = Arc::new(StubVerification::ok("$flow-unused"));
    let app = verify_app(store, verify.clone());

    let account_id = Uuid::new_v4();
    let (status, body) = request(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/verify"),
        Some(json!({})),
        Some(&bearer()),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("neither a device_id nor a user_id"));
    assert!(verify.calls().is_empty());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn verify_get_returns_flow_state_dto() {
    let store = store().await;
    let verify = Arc::new(StubVerification::ok("$flow-xyz"));
    let app = verify_app(store, verify.clone());
    let account_id = Uuid::new_v4();

    let (status, body) = get(&app, &format!("/v1/accounts/{account_id}/verify/$flow-xyz")).await;

    assert_eq!(status, StatusCode::OK);
    let flow = &body["data"];
    assert_eq!(flow["flow_id"], "$flow-xyz");
    assert_eq!(flow["device_id"], "TRUSTEDDEV");
    assert_eq!(flow["stage"], "keys_exchanged");
    assert_eq!(flow["emoji"][0]["symbol"], "🐶");
    assert_eq!(flow["emoji"][0]["description"], "Dog");
    assert_eq!(flow["decimals"][0], 1234);
    assert_eq!(flow["decimals"][2], 9012);
    assert!(flow["cancel_reason"].is_null());

    assert_eq!(
        verify.calls(),
        vec![VerifyCall::Get {
            account_id,
            flow_id: "$flow-xyz".to_owned(),
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn verify_list_returns_flows() {
    let store = store().await;
    let verify = Arc::new(StubVerification::ok("$flow-1"));
    let app = verify_app(store, verify.clone());
    let account_id = Uuid::new_v4();

    let (status, body) = get(&app, &format!("/v1/accounts/{account_id}/verify")).await;

    assert_eq!(status, StatusCode::OK);
    let flows = body["data"].as_array().expect("data array");
    assert_eq!(flows.len(), 1);
    assert_eq!(flows[0]["flow_id"], "$flow-1");
    assert_eq!(verify.calls(), vec![VerifyCall::List { account_id }]);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn verify_confirm_and_cancel_return_204_and_route() {
    let store = store().await;
    let verify = Arc::new(StubVerification::ok("$flow-2"));
    let app = verify_app(store, verify.clone());
    let account_id = Uuid::new_v4();

    let (status, _) = request(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/verify/$flow-2/confirm"),
        None,
        Some(&bearer()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = request(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/verify/$flow-2/cancel"),
        None,
        Some(&bearer()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    assert_eq!(
        verify.calls(),
        vec![
            VerifyCall::Confirm {
                account_id,
                flow_id: "$flow-2".to_owned(),
            },
            VerifyCall::Cancel {
                account_id,
                flow_id: "$flow-2".to_owned(),
            },
        ]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn verify_error_maps_to_status() {
    let store = store().await;
    let account_id = Uuid::new_v4();
    let body = json!({ "device_id": "D" });

    let cases = [
        (
            VerifyOutcome::NotFound("nope".into()),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            VerifyOutcome::NotActive("logged out".into()),
            StatusCode::CONFLICT,
            "conflict",
        ),
        (
            VerifyOutcome::Conflict("wrong stage".into()),
            StatusCode::CONFLICT,
            "conflict",
        ),
        (
            VerifyOutcome::BadRequest("unknown device".into()),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            VerifyOutcome::Upstream("hs down".into()),
            StatusCode::BAD_GATEWAY,
            "bad_gateway",
        ),
        (
            VerifyOutcome::Internal,
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
        ),
    ];

    for (outcome, want_status, want_code) in cases {
        let app = verify_app(store.clone(), Arc::new(StubVerification::failing(outcome)));
        let (status, err) = request(
            &app,
            "POST",
            &format!("/v1/accounts/{account_id}/verify"),
            Some(body.clone()),
            Some(&bearer()),
        )
        .await;
        assert_eq!(status, want_status);
        assert_eq!(err["error"]["code"], want_code);
    }
}

// ---- Per-event verification bundle (M7c) ----

#[tokio::test]
#[ignore = "requires Postgres"]
async fn verification_bundle_returns_snapshot_and_current() {
    let store = store().await;
    let app = trust_app(store, Arc::new(StubTrust::ok()));
    let account_id = Uuid::new_v4();

    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/events/$evt:localhost/verification"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["event_id"], "$evt:localhost");
    assert_eq!(body["data"]["sender"], "@bob:localhost");
    // The at-decrypt snapshot half.
    assert_eq!(body["data"]["snapshot"]["sender_trust"], "verified");
    assert_eq!(body["data"]["snapshot"]["device_id"], "BOBDEVICE");
    // Megolm session provenance rides the snapshot half.
    assert_eq!(body["data"]["snapshot"]["session_id"], "session-1");
    assert_eq!(body["data"]["snapshot"]["forwarded"], false);
    // The live-evidence half — separate from the snapshot.
    assert_eq!(body["data"]["current"]["device_cross_signed"], true);
    assert_eq!(body["data"]["current"]["identity_verified"], true);
    assert_eq!(body["data"]["current"]["verification_violation"], false);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn verification_bundle_error_maps_to_status() {
    let store = store().await;
    let account_id = Uuid::new_v4();

    let cases = [
        (
            (|| axon_api::TrustError::NotFound("no event".into())) as fn() -> axon_api::TrustError,
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            || axon_api::TrustError::NotActive("logged out".into()),
            StatusCode::CONFLICT,
            "conflict",
        ),
        (
            || axon_api::TrustError::Upstream("hs down".into()),
            StatusCode::BAD_GATEWAY,
            "bad_gateway",
        ),
        (
            || axon_api::TrustError::Internal,
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
        ),
    ];

    for (make_err, want_status, want_code) in cases {
        let app = trust_app(store.clone(), Arc::new(StubTrust::failing(make_err)));
        let (status, err) = get(
            &app,
            &format!("/v1/accounts/{account_id}/events/$evt:localhost/verification"),
        )
        .await;
        assert_eq!(status, want_status);
        assert_eq!(err["error"]["code"], want_code);
    }
}

// ---- Device-list / discovery (M16, ADR 0060) ----

#[tokio::test]
#[ignore = "requires Postgres"]
async fn list_devices_defaults_to_own_user() {
    let store = store().await;
    let app = devices_app(store, Arc::new(StubDeviceList::ok()));
    let account_id = Uuid::new_v4();

    let (status, body) = get(&app, &format!("/v1/accounts/{account_id}/devices")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["user_id"], "@alice:localhost");
    let devices = body["data"]["devices"].as_array().expect("devices array");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["device_id"], "ALICEDEVICE");
    assert_eq!(devices[0]["display_name"], "Alice's Phone");
    assert_eq!(devices[0]["is_verified"], false);
    assert_eq!(devices[0]["is_cross_signed_by_owner"], false);
    assert_eq!(devices[0]["local_trust_state"], "unset");
    assert_eq!(devices[0]["algorithms"][0], "m.megolm.v1.aes-sha2");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn list_devices_with_explicit_user_id() {
    let store = store().await;
    let stub = Arc::new(StubDeviceList::ok());
    let app = devices_app(store, stub.clone());
    let account_id = Uuid::new_v4();

    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/devices?user_id=@bob:localhost"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    // The stub always returns its canned list regardless of the requested
    // user; what this test asserts is that the query param actually reached
    // the port.
    assert_eq!(stub.calls(), vec![Some("@bob:localhost".to_owned())]);
    assert!(body["data"]["devices"].as_array().is_some());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn list_devices_empty_is_200_not_404() {
    let store = store().await;
    let stub = Arc::new(StubDeviceList::empty("@nobody:localhost"));
    let app = devices_app(store, stub);
    let account_id = Uuid::new_v4();

    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/devices?user_id=@nobody:localhost"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["devices"].as_array().unwrap().len(), 0);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn list_devices_error_maps_to_status() {
    let store = store().await;
    let account_id = Uuid::new_v4();

    let cases = [
        (
            (|| axon_api::DeviceListError::NotFound("no account".into()))
                as fn() -> axon_api::DeviceListError,
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            || axon_api::DeviceListError::NotActive("logged out".into()),
            StatusCode::CONFLICT,
            "conflict",
        ),
        (
            || axon_api::DeviceListError::BadRequest("bad user id".into()),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            || axon_api::DeviceListError::Upstream("hs down".into()),
            StatusCode::BAD_GATEWAY,
            "bad_gateway",
        ),
        (
            || axon_api::DeviceListError::Internal,
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
        ),
    ];

    for (make_err, want_status, want_code) in cases {
        let app = devices_app(store.clone(), Arc::new(StubDeviceList::failing(make_err)));
        let (status, err) = get(&app, &format!("/v1/accounts/{account_id}/devices")).await;
        assert_eq!(status, want_status);
        assert_eq!(err["error"]["code"], want_code);
    }
}

/// M8b: the relation-aggregation read endpoints resolve over relations that land
/// far outside the default timeline window, and the timeline read serves the
/// collapsed/edited view. Exercises reactions (tally, dedup, `me`, redaction),
/// the collapsed timeline (edited body in place, no stray edit rows), the edits
/// trail, replies, and threads + a thread-scoped paginated timeline.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn aggregation_api_end_to_end() {
    let store = store().await;
    let pool = store.pool().clone();
    let me_user = format!("@agg-{}:localhost", Uuid::new_v4());
    let account_id = store
        .upsert_account(&me_user, "https://hs.example.org")
        .await
        .expect("account")
        .account_id;
    let room_id = format!("!agg-{}:localhost", Uuid::new_v4());

    // The base message at ts=1000; every relation below lands *far later* (ts 2k–9k)
    // so a naive client window over the newest page would miss them.
    let msg = insert_message(&store, account_id, &room_id, 1_000, "hello").await;

    // --- Reactions: 👍 from alice (twice → dedup) + from me; ❤️ from bob; a 👎
    // from carol that gets redacted (drops from the tally, leaving the rest). ---
    let react = |sender: &'static str, ts: i64, target: String, key: &'static str| {
        let store = store.clone();
        let room_id = room_id.clone();
        async move {
            insert_relation(
                &store,
                account_id,
                &room_id,
                sender,
                ts,
                "m.reaction",
                json!({}),
                json!({ "rel_type": "m.annotation", "event_id": target, "key": key }),
            )
            .await
        }
    };
    react("@alice:localhost", 9_000, msg.clone(), "👍").await;
    react("@alice:localhost", 9_001, msg.clone(), "👍").await; // duplicate (sender,key)
    let my_thumb = insert_relation(
        &store,
        account_id,
        &room_id,
        &me_user,
        9_002,
        "m.reaction",
        json!({}),
        json!({ "rel_type": "m.annotation", "event_id": msg.clone(), "key": "👍" }),
    )
    .await;
    react("@bob:localhost", 9_003, msg.clone(), "❤️").await;
    let downvote = react("@carol:localhost", 9_004, msg.clone(), "👎").await;
    insert_redaction(&store, account_id, &room_id, 9_005, &downvote).await;

    let app = read_app(store.clone());

    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/events/{msg}/reactions"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tally = &body["data"];
    assert_eq!(tally["👍"]["count"], 2, "alice + me, the dup counts once");
    assert_eq!(tally["👍"]["me"], true);
    let senders = tally["👍"]["senders"].as_array().expect("senders");
    assert!(senders.iter().any(|s| s == "@alice:localhost"));
    assert!(senders.iter().any(|s| s == me_user.as_str()));
    assert_eq!(tally["❤️"]["count"], 1);
    assert_eq!(tally["❤️"]["me"], false);
    assert!(tally.get("👎").is_none(), "redacted reaction drops out");
    // my_event_ids carries the account user's own reaction event(s) for the key —
    // the ids a client redacts to unreact — and is empty for keys we didn't send.
    let mine = tally["👍"]["my_event_ids"]
        .as_array()
        .expect("my_event_ids");
    assert_eq!(mine.len(), 1, "only my own 👍 reaction event");
    assert_eq!(mine[0], my_thumb.as_str());
    assert_eq!(
        tally["❤️"]["my_event_ids"],
        json!([]),
        "no own reaction event for a key we didn't send"
    );

    // An event with no reactions → empty object, not a 404.
    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/events/$nobody:localhost/reactions"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));

    // --- Edits: two valid edits by the original sender (latest wins) + one from a
    // non-sender (ignored by the collapse, but present in the forensic trail). ---
    let edit = |sender: &'static str, ts: i64, new_body: &'static str| {
        let store = store.clone();
        let room_id = room_id.clone();
        let target = msg.clone();
        async move {
            insert_relation(
                &store,
                account_id,
                &room_id,
                sender,
                ts,
                "m.room.message",
                json!({
                    "msgtype": "m.text",
                    "body": format!("* {new_body}"),
                    "m.new_content": { "msgtype": "m.text", "body": new_body },
                }),
                json!({ "rel_type": "m.replace", "event_id": target }),
            )
            .await
        }
    };
    edit("@alice:localhost", 2_000, "hello (v2)").await;
    edit("@alice:localhost", 3_000, "hello (v3)").await; // latest valid edit
    edit("@mallory:localhost", 4_000, "PWNED").await; // non-sender → not honored

    // Timeline serves the collapsed/edited view: msg's body is the latest edit,
    // `edited`/`edit_count` are set, and no standalone m.replace rows appear.
    let (status, page) = get(
        &app,
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/timeline"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = page["data"]["events"].as_array().expect("events");
    let m = events
        .iter()
        .find(|e| e["event_id"] == msg.as_str())
        .expect("base message present in timeline");
    assert_eq!(m["body"], "hello (v3)", "non-sender edit must not win");
    assert_eq!(m["edited"], true);
    assert_eq!(m["edit_count"], 2, "two valid edits; mallory's is excluded");
    assert!(
        events
            .iter()
            .all(|e| e["relates_to"]["rel_type"] != "m.replace"),
        "standalone edit rows must be collapsed out of the timeline"
    );

    // The forensic edits trail is unfiltered (includes the non-sender edit), oldest
    // first.
    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/events/{msg}/edits"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let edits = body["data"].as_array().expect("edits");
    assert_eq!(edits.len(), 3, "two valid + one non-sender, all preserved");
    assert_eq!(edits[0]["body"], "* hello (v2)", "oldest first");

    // --- Replies: a plain reply (no rel_type) is found via nested m.in_reply_to;
    // thread members (below) must not bleed into it. ---
    let reply = insert_relation(
        &store,
        account_id,
        &room_id,
        "@bob:localhost",
        5_000,
        "m.room.message",
        json!({ "msgtype": "m.text", "body": "a reply" }),
        json!({ "m.in_reply_to": { "event_id": msg.clone() } }),
    )
    .await;
    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/events/{msg}/replies"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let replies = body["data"].as_array().expect("replies");
    assert_eq!(replies.len(), 1, "only the plain reply, not thread members");
    assert_eq!(replies[0]["event_id"], reply.as_str());
    assert_eq!(replies[0]["body"], "a reply");

    // --- Threads: two thread replies rooted at msg → one thread, reply_count 2,
    // latest is the newest; the thread timeline pages reverse-chronologically. ---
    let thread = |ts: i64, body: &'static str| {
        let store = store.clone();
        let room_id = room_id.clone();
        let root = msg.clone();
        async move {
            insert_relation(
                &store,
                account_id,
                &room_id,
                "@bob:localhost",
                ts,
                "m.room.message",
                json!({ "msgtype": "m.text", "body": body }),
                json!({
                    "rel_type": "m.thread",
                    "event_id": root,
                    "m.in_reply_to": { "event_id": root, "is_falling_back": true },
                }),
            )
            .await
        }
    };
    let t1 = thread(6_000, "thread 1").await;
    let t2 = thread(6_001, "thread 2").await;

    let (status, body) = get(
        &app,
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/threads"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let threads = body["data"].as_array().expect("threads");
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0]["root_event_id"], msg.as_str());
    assert_eq!(threads[0]["reply_count"], 2);
    assert_eq!(threads[0]["latest_reply_event_id"], t2.as_str());
    assert_eq!(threads[0]["latest_reply_ts"], 6_001);

    // Thread-scoped timeline, paginated: page 1 (limit 1) → [t2] + cursor;
    // page 2 → [t1]; only thread members appear, newest first.
    let base = format!("/v1/accounts/{account_id}/rooms/{room_id}/threads/{msg}/timeline");
    let (status, page1) = get(&app, &format!("{base}?limit=1")).await;
    assert_eq!(status, StatusCode::OK);
    let evs = page1["data"]["events"].as_array().expect("events");
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0]["event_id"], t2.as_str());
    let cursor = page1["data"]["next_cursor"].as_str().expect("next_cursor");

    let (status, page2) = get(&app, &format!("{base}?limit=1&cursor={cursor}")).await;
    assert_eq!(status, StatusCode::OK);
    let evs2 = page2["data"]["events"].as_array().expect("events");
    assert_eq!(evs2.len(), 1);
    assert_eq!(evs2[0]["event_id"], t1.as_str());

    // An unknown thread root → an empty page, not a 404.
    let (status, empty) = get(
        &app,
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/threads/$nope:localhost/timeline"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty["data"]["events"].as_array().expect("events").len(), 0);

    // A malformed cursor on the thread timeline is a 400, like the room timeline.
    let (status, err) = get(&app, &format!("{base}?cursor=not-a-cursor")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["code"], "bad_request");

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

/// Regression for the unreact round-trip after relation aggregation (Adam's M8b
/// review, finding 1): the collapsed timeline strips raw `m.reaction` rows, so a
/// client can no longer recover its own reaction event ids by scanning events. It
/// must instead read them from the aggregated tally's `my_event_ids`. This walks
/// react → reload the timeline → withdraw (redact the id from `my_event_ids`) →
/// reload again and confirm the reaction is gone.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn unreact_uses_aggregated_reaction_ids_from_timeline() {
    let store = store().await;
    let pool = store.pool().clone();
    let me_user = format!("@unreact-{}:localhost", Uuid::new_v4());
    let account_id = store
        .upsert_account(&me_user, "https://hs.example.org")
        .await
        .expect("account")
        .account_id;
    let room_id = format!("!unreact-{}:localhost", Uuid::new_v4());

    let msg = insert_message(&store, account_id, &room_id, 1_000, "hello").await;
    // I react with 👍; the raw reaction lands far later than the message.
    let my_reaction = insert_relation(
        &store,
        account_id,
        &room_id,
        &me_user,
        9_000,
        "m.reaction",
        json!({}),
        json!({ "rel_type": "m.annotation", "event_id": msg.clone(), "key": "👍" }),
    )
    .await;

    let app = read_app(store.clone());
    let timeline = format!("/v1/accounts/{account_id}/rooms/{room_id}/timeline");

    // Reload the timeline: the raw reaction row is gone, but the aggregated tally
    // on the message carries my_event_ids — the id a client redacts to unreact.
    let (status, page) = get(&app, &timeline).await;
    assert_eq!(status, StatusCode::OK);
    let events = page["data"]["events"].as_array().expect("events");
    assert!(
        !events.iter().any(|e| e["event_id"] == my_reaction.as_str()),
        "raw m.reaction must not appear in the collapsed timeline"
    );
    let m = events
        .iter()
        .find(|e| e["event_id"] == msg.as_str())
        .expect("base message present");
    assert_eq!(m["reactions"]["👍"]["count"], 1);
    assert_eq!(m["reactions"]["👍"]["me"], true);
    let mine = m["reactions"]["👍"]["my_event_ids"]
        .as_array()
        .expect("my_event_ids");
    assert_eq!(mine, &[json!(my_reaction)], "the id to redact to unreact");

    // Withdraw: redact exactly the id the client read from my_event_ids.
    insert_redaction(&store, account_id, &room_id, 9_001, &my_reaction).await;

    // Reload again: the reaction is gone from the message's tally entirely.
    let (status, page) = get(&app, &timeline).await;
    assert_eq!(status, StatusCode::OK);
    let m = page["data"]["events"]
        .as_array()
        .expect("events")
        .iter()
        .find(|e| e["event_id"] == msg.as_str())
        .expect("base message present")
        .clone();
    assert!(
        m["reactions"].is_null() || m["reactions"].get("👍").is_none(),
        "withdrawn reaction drops from the tally"
    );

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

/// M12 device-state (`/v1/devices/{device_id}/state/{namespace}`): PUT
/// merge-upserts and GET returns the cross-device LWW-merged view; `null`
/// tombstones a key; parameter errors are readable 400s/404s.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn device_state_put_get_merge_and_tombstone() {
    let store = store().await;
    let pool = store.pool().clone();
    let account_id = store
        .upsert_account(
            &format!("@devstate-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .expect("account")
        .account_id;
    let app = read_app(store.clone());
    let device_a = Uuid::new_v4();
    let device_b = Uuid::new_v4();
    let uri = |device: Uuid| format!("/v1/devices/{device}/state/drafts?account_id={account_id}");

    // A namespace never written reads as an empty map, not a 404.
    let (status, body) = get(&app, &uri(device_a)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["namespace"], "drafts");
    assert_eq!(body["data"]["entries"], json!({}));

    // Device A writes two room drafts in one merge-upsert.
    let (status, body) = request(
        &app,
        "PUT",
        &uri(device_a),
        Some(json!({ "entries": {
            "!one:localhost": { "text": "draft one" },
            "!two:localhost": { "text": "draft two" },
        }})),
        Some(&bearer()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["data"]["updated_at"].is_string());

    // Device B overwrites one key later; the merged GET shows B's value winning
    // and A's untouched key surviving (merge, not replace).
    let (status, _) = request(
        &app,
        "PUT",
        &uri(device_b),
        Some(json!({ "entries": { "!one:localhost": { "text": "B wins" } } })),
        Some(&bearer()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = get(&app, &uri(device_a)).await;
    assert_eq!(status, StatusCode::OK);
    let entries = &body["data"]["entries"];
    assert_eq!(entries["!one:localhost"]["value"]["text"], "B wins");
    assert_eq!(entries["!one:localhost"]["device_id"], device_b.to_string());
    assert_eq!(entries["!two:localhost"]["value"]["text"], "draft two");
    assert_eq!(entries["!two:localhost"]["device_id"], device_a.to_string());

    // A null entry tombstones the key: gone from the merged view even though
    // device A's older row still exists underneath.
    let (status, _) = request(
        &app,
        "PUT",
        &uri(device_b),
        Some(json!({ "entries": { "!two:localhost": null } })),
        Some(&bearer()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = get(&app, &uri(device_a)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["data"]["entries"].get("!two:localhost").is_none());

    // Parameter errors: missing account_id → 400; unknown account → 404;
    // malformed device UUID → 400; empty entries → 400; no token → 401.
    let (status, body) = get(&app, &format!("/v1/devices/{device_a}/state/drafts")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request");

    let unknown = Uuid::new_v4();
    let (status, body) = get(
        &app,
        &format!("/v1/devices/{device_a}/state/drafts?account_id={unknown}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");

    let (status, _) = get(
        &app,
        &format!("/v1/devices/not-a-uuid/state/drafts?account_id={account_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = request(
        &app,
        "PUT",
        &uri(device_a),
        Some(json!({ "entries": {} })),
        Some(&bearer()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request");

    let (status, _) = request(&app, "GET", &uri(device_a), None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

/// M12 device-state write caps (ADR 0048): values are opaque, so the handler
/// bounds size, not shape — oversized values/keys/namespaces and over-long
/// entry lists are readable 400s, and boundary-sized writes still land.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn device_state_put_rejects_oversized_writes() {
    let store = store().await;
    let pool = store.pool().clone();
    let account_id = store
        .upsert_account(
            &format!("@devstate-caps-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .expect("account")
        .account_id;
    let app = read_app(store.clone());
    let device_id = Uuid::new_v4();
    let uri = format!("/v1/devices/{device_id}/state/drafts?account_id={account_id}");
    let auth = bearer();
    let put = |body: Value| request(&app, "PUT", &uri, Some(body), Some(auth.as_str()));

    // A value over 64 KiB serialized is refused…
    let big = "x".repeat(64 * 1024 + 1);
    let (status, body) = put(json!({ "entries": { "!r:hs": { "text": big } } })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request");

    // …while a comfortably large draft still lands.
    let fits = "x".repeat(32 * 1024);
    let (status, _) = put(json!({ "entries": { "!r:hs": { "text": fits } } })).await;
    assert_eq!(status, StatusCode::OK);

    // More than 64 entries in one merge-upsert is refused; 64 is accepted.
    let too_many: serde_json::Map<String, Value> = (0..65)
        .map(|i| (format!("!room-{i}:hs"), json!({ "text": "x" })))
        .collect();
    let (status, _) = put(json!({ "entries": too_many })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let at_cap: serde_json::Map<String, Value> = (0..64)
        .map(|i| (format!("!room-{i}:hs"), json!({ "text": "x" })))
        .collect();
    let (status, _) = put(json!({ "entries": at_cap })).await;
    assert_eq!(status, StatusCode::OK);

    // A key over 512 bytes is refused.
    let long_key = format!("!{}:hs", "k".repeat(513));
    let (status, _) = put(json!({ "entries": { long_key: { "text": "x" } } })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A namespace over 64 bytes is refused on write (reads stay unrestricted —
    // an unknown namespace is just an empty map).
    let long_ns = "n".repeat(65);
    let ns_uri = format!("/v1/devices/{device_id}/state/{long_ns}?account_id={account_id}");
    let (status, _) = request(
        &app,
        "PUT",
        &ns_uri,
        Some(json!({ "entries": { "!r:hs": { "text": "x" } } })),
        Some(&bearer()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = get(&app, &ns_uri).await;
    assert_eq!(status, StatusCode::OK);

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}
