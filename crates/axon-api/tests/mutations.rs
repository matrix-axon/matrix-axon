//! Handler tests for the M6 mutation routes.
//!
//! These drive the real router with a [`StubSender`] in place of the SDK
//! gateway, so they assert routing, request decoding, the success envelope, and
//! `SendError` → HTTP-status mapping **without** a homeserver. `AppState` needs a
//! `Store` to build (the read handlers share the router), so like the other API
//! tests they connect to Postgres and are `#[ignore]`d by default — the store is
//! never queried by these handlers:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-api --test mutations -- --ignored
//! ```

mod common;

use std::sync::Arc;

use axon_api::{AppState, MediaAttachment, MediaSendKind, MessageSender, StagedUploadService};
use axon_store::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    Call, Outcome, StubDeviceList, StubLifecycle, StubMediaProxy, StubSender, StubTokenVerifier,
    StubTrust, StubUploads, StubVerification, TEST_TOKEN,
};
use serde_json::{json, Value};
use tower::ServiceExt; // for `oneshot`
use uuid::Uuid;

async fn store() -> Store {
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
    Store::connect(&url, 5).await.expect("connect + migrate")
}

/// Build the router around a given sender. The live bus and lifecycle port are
/// unused here.
fn app(store: Store, sender: Arc<dyn MessageSender>) -> axum::Router {
    app_with_uploads(store, sender, Arc::new(StubUploads::ok()))
}

fn app_with_uploads(
    store: Store,
    sender: Arc<dyn MessageSender>,
    uploads: Arc<dyn StagedUploadService>,
) -> axum::Router {
    let (live, _rx) = tokio::sync::broadcast::channel(16);
    let lifecycle = Arc::new(StubLifecycle::ok(Uuid::nil()));
    let verify = Arc::new(StubVerification::ok("$unused-flow"));
    let trust = Arc::new(StubTrust::ok());
    let devices = Arc::new(StubDeviceList::ok());
    let verifier = Arc::new(StubTokenVerifier::ok());
    let media = Arc::new(StubMediaProxy);
    axon_api::router(
        AppState::new(
            store, live, sender, lifecycle, verify, trust, devices, verifier, media, None,
        )
        .with_staged_uploads(uploads),
    )
}

/// Issue a request (optional JSON body) and return `(status, parsed body)`. Every
/// request carries the bearer token the auth gate (M7b) requires.
async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {TEST_TOKEN}"));
    let req = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => {
            builder = builder.header("content-length", "0");
            builder.body(Body::empty()).unwrap()
        }
    };
    let resp = app.clone().oneshot(req).await.expect("request");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, json)
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn mutations_route_to_sender_and_envelope_result() {
    let store = store().await;
    let stub = Arc::new(StubSender::ok("$created:localhost"));
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";
    let event_id = "$target:localhost";

    // send
    let (status, body) = send(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/send"),
        Some(json!({ "body": "hello" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["event_id"], "$created:localhost");

    // edit
    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}"),
        Some(json!({ "body": "edited" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // redact with a reason
    let (status, _) = send(
        &app,
        "DELETE",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}?reason=spam"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // react
    let (status, _) = send(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}/reactions"),
        Some(json!({ "key": "👍" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Each handler routed to the matching MessageSender call with the path/body args.
    assert_eq!(
        stub.calls(),
        vec![
            Call::Send {
                account_id,
                room_id: room_id.to_owned(),
                body: "hello".to_owned(),
                formatted: None,
                reply_to: None,
                thread_root: None,
            },
            Call::Edit {
                account_id,
                room_id: room_id.to_owned(),
                event_id: event_id.to_owned(),
                body: "edited".to_owned(),
                formatted: None,
            },
            Call::Redact {
                account_id,
                room_id: room_id.to_owned(),
                event_id: event_id.to_owned(),
                reason: Some("spam".to_owned()),
            },
            Call::React {
                account_id,
                room_id: room_id.to_owned(),
                event_id: event_id.to_owned(),
                key: "👍".to_owned(),
            },
        ]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn send_media_claims_sends_and_completes_upload() {
    let store = store().await;
    let sender = Arc::new(StubSender::ok("$media:localhost"));
    let uploads = Arc::new(StubUploads::ok());
    let app = app_with_uploads(store, sender.clone(), uploads.clone());

    let account_id = Uuid::new_v4();
    let upload_id = Uuid::new_v4();
    let room_id = "!room:localhost";

    let (status, body) = send(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/send-media"),
        Some(json!({
            "upload_id": upload_id,
            "caption": "look",
            "reply_to": "$reply:localhost",
            "thread_root": "$root:localhost",
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["event_id"], "$media:localhost");
    assert_eq!(uploads.claims(), vec![(account_id, upload_id)]);
    assert_eq!(uploads.completes(), vec![(account_id, upload_id)]);
    assert!(uploads.releases().is_empty());
    assert_eq!(
        sender.calls(),
        vec![Call::SendMedia {
            account_id,
            room_id: room_id.to_owned(),
            attachment: MediaAttachment {
                kind: MediaSendKind::Image,
                filename: "photo.png".to_owned(),
                content_type: Some("image/png".to_owned()),
                size_bytes: 3,
                bytes: b"abc".to_vec(),
            },
            caption: Some("look".to_owned()),
            formatted: None,
            reply_to: Some("$reply:localhost".to_owned()),
            thread_root: Some("$root:localhost".to_owned()),
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn send_media_releases_upload_after_sender_failure() {
    let store = store().await;
    let sender = Arc::new(StubSender::failing(Outcome::Upstream("homeserver".into())));
    let uploads = Arc::new(StubUploads::ok());
    let app = app_with_uploads(store, sender, uploads.clone());

    let account_id = Uuid::new_v4();
    let upload_id = Uuid::new_v4();
    let (status, body) = send(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/rooms/!room:localhost/send-media"),
        Some(json!({ "upload_id": upload_id })),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"]["code"], "bad_gateway");
    assert_eq!(uploads.claims(), vec![(account_id, upload_id)]);
    assert!(uploads.completes().is_empty());
    assert_eq!(uploads.releases(), vec![(account_id, upload_id)]);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn send_media_formatted_caption_reaches_the_sender() {
    let store = store().await;
    let sender = Arc::new(StubSender::ok("$media:localhost"));
    let uploads = Arc::new(StubUploads::ok());
    let app = app_with_uploads(store, sender.clone(), uploads.clone());

    let account_id = Uuid::new_v4();
    let upload_id = Uuid::new_v4();
    let (status, _) = send(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/rooms/!room:localhost/send-media"),
        Some(json!({
            "upload_id": upload_id,
            "caption": "a **cat**",
            "format": "org.matrix.custom.html",
            "formatted_body": "a <strong>cat</strong>",
        })),
    )
    .await;

    // Issue #237: the caption's HTML is no longer dropped at the API boundary.
    assert_eq!(status, StatusCode::OK);
    let calls = sender.calls();
    let [Call::SendMedia {
        caption, formatted, ..
    }] = calls.as_slice()
    else {
        panic!("expected one media send, got {calls:?}");
    };
    assert_eq!(caption.as_deref(), Some("a **cat**"));
    assert_eq!(
        formatted.clone(),
        Some((
            "org.matrix.custom.html".to_owned(),
            "a <strong>cat</strong>".to_owned(),
        ))
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn send_media_invalid_formatting_is_400_and_keeps_the_upload() {
    let store = store().await;
    let sender = Arc::new(StubSender::ok("$media:localhost"));
    let uploads = Arc::new(StubUploads::ok());
    let app = app_with_uploads(store, sender.clone(), uploads.clone());
    let account_id = Uuid::new_v4();
    let uri = format!("/v1/accounts/{account_id}/rooms/!room:localhost/send-media");

    for body in [
        // Formatting with no caption: Matrix formats only a caption.
        json!({
            "upload_id": Uuid::new_v4(),
            "format": "org.matrix.custom.html",
            "formatted_body": "<strong>hi</strong>",
        }),
        // Half of the pair.
        json!({
            "upload_id": Uuid::new_v4(),
            "caption": "hi",
            "formatted_body": "<strong>hi</strong>",
        }),
        // An unrecognized format.
        json!({
            "upload_id": Uuid::new_v4(),
            "caption": "hi",
            "format": "text/markdown",
            "formatted_body": "**hi**",
        }),
    ] {
        let (status, err) = send(&app, "POST", &uri, Some(body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(err["error"]["code"], "bad_request", "{body}");
    }

    // Rejected before the claim, so no staged upload was consumed or released.
    assert!(uploads.claims().is_empty());
    assert!(uploads.releases().is_empty());
    assert!(sender.calls().is_empty());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn send_error_maps_to_status() {
    let store = store().await;
    let account_id = Uuid::new_v4();
    let uri = format!("/v1/accounts/{account_id}/rooms/!r:localhost/send");

    let cases = [
        (
            Outcome::NotFound("x".into()),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            Outcome::Forbidden("x".into()),
            StatusCode::FORBIDDEN,
            "forbidden",
        ),
        (
            Outcome::Unavailable("x".into()),
            StatusCode::SERVICE_UNAVAILABLE,
            "service_unavailable",
        ),
        (
            Outcome::Invalid("x".into()),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            Outcome::Upstream("x".into()),
            StatusCode::BAD_GATEWAY,
            "bad_gateway",
        ),
        (
            // ADR 0030 decision #4, issue #241: a send-mutation timeout is a
            // distinct 504, not folded into the 502 `Upstream` case above.
            Outcome::Timeout("x".into()),
            StatusCode::GATEWAY_TIMEOUT,
            "gateway_timeout",
        ),
    ];

    for (outcome, want_status, want_code) in cases {
        let app = app(store.clone(), Arc::new(StubSender::failing(outcome)));
        let (status, body) = send(&app, "POST", &uri, Some(json!({ "body": "hi" }))).await;
        assert_eq!(status, want_status);
        assert_eq!(body["error"]["code"], want_code);
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn malformed_body_is_enveloped_400_and_skips_sender() {
    let store = store().await;
    let stub = Arc::new(StubSender::ok("$created:localhost"));
    let app = app(store, stub.clone());
    let account_id = Uuid::new_v4();

    // Missing the required `body` field → JSON decode failure.
    let (status, err) = send(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/rooms/!r:localhost/send"),
        Some(json!({ "wrong": "field" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["code"], "bad_request");
    // The body never decoded, so the sender was not invoked.
    assert!(stub.calls().is_empty());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn formatted_fields_reach_the_sender() {
    let store = store().await;
    let stub = Arc::new(StubSender::ok("$created:localhost"));
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";
    let event_id = "$target:localhost";

    // send with format + formatted_body
    let (status, _) = send(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/send"),
        Some(json!({
            "body": "hello",
            "format": "org.matrix.custom.html",
            "formatted_body": "<strong>hello</strong>",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // edit with format + formatted_body
    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}"),
        Some(json!({
            "body": "edited",
            "format": "org.matrix.custom.html",
            "formatted_body": "<em>edited</em>",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        stub.calls(),
        vec![
            Call::Send {
                account_id,
                room_id: room_id.to_owned(),
                body: "hello".to_owned(),
                formatted: Some((
                    "org.matrix.custom.html".to_owned(),
                    "<strong>hello</strong>".to_owned(),
                )),
                reply_to: None,
                thread_root: None,
            },
            Call::Edit {
                account_id,
                room_id: room_id.to_owned(),
                event_id: event_id.to_owned(),
                body: "edited".to_owned(),
                formatted: Some((
                    "org.matrix.custom.html".to_owned(),
                    "<em>edited</em>".to_owned(),
                )),
            },
        ]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn reply_and_thread_fields_reach_the_sender() {
    let store = store().await;
    let stub = Arc::new(StubSender::ok("$created:localhost"));
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";

    // A plain reply: reply_to set, thread_root absent.
    let (status, _) = send(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/send"),
        Some(json!({ "body": "a reply", "reply_to": "$target:localhost" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A thread message: thread_root set (reply_to may also be set).
    let (status, _) = send(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/send"),
        Some(json!({ "body": "in thread", "thread_root": "$root:localhost" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        stub.calls(),
        vec![
            Call::Send {
                account_id,
                room_id: room_id.to_owned(),
                body: "a reply".to_owned(),
                formatted: None,
                reply_to: Some("$target:localhost".to_owned()),
                thread_root: None,
            },
            Call::Send {
                account_id,
                room_id: room_id.to_owned(),
                body: "in thread".to_owned(),
                formatted: None,
                reply_to: None,
                thread_root: Some("$root:localhost".to_owned()),
            },
        ]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn invalid_formatting_is_400_and_skips_sender() {
    let store = store().await;
    let stub = Arc::new(StubSender::ok("$created:localhost"));
    let app = app(store, stub.clone());
    let account_id = Uuid::new_v4();
    let uri = format!("/v1/accounts/{account_id}/rooms/!r:localhost/send");

    // formatted_body without format → both must be present together.
    let (status, err) = send(
        &app,
        "POST",
        &uri,
        Some(json!({ "body": "hi", "formatted_body": "<b>hi</b>" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["code"], "bad_request");

    // an unrecognized format value is rejected.
    let (status, err) = send(
        &app,
        "POST",
        &uri,
        Some(json!({
            "body": "hi",
            "format": "text/markdown",
            "formatted_body": "**hi**",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["code"], "bad_request");

    // Neither malformed request reached the sender.
    assert!(stub.calls().is_empty());
}
