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

#[tokio::test]
#[ignore = "requires Postgres"]
async fn later_preview_claim_failure_releases_only_earlier_owned_claims() {
    let sender = Arc::new(StubSender::ok("$must-not-send"));
    let uploads = Arc::new(StubUploads::ok());
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    uploads.reject_claim_for(second);
    let app = app_with_uploads(store().await, sender.clone(), uploads.clone());
    let account = Uuid::new_v4();
    let (status, _) = send(&app,"POST",&format!("/v1/accounts/{account}/rooms/!room:localhost/send"),Some(json!({"body":"https://example.org/a https://example.org/b","url_previews":[{"matched_url":"https://example.org/a","image_upload_id":first},{"matched_url":"https://example.org/b","image_upload_id":second}]}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(uploads.claims(), vec![(account, first), (account, second)]);
    assert_eq!(uploads.releases(), vec![(account, first)]);
    assert!(uploads.completes().is_empty());
    assert!(sender.calls().is_empty());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn bundled_previews_are_explicit_and_consumed_with_the_parent_message() {
    let sender = Arc::new(StubSender::ok("$preview"));
    let uploads = Arc::new(StubUploads::ok());
    let app = app_with_uploads(store().await, sender.clone(), uploads.clone());
    let account = Uuid::new_v4();
    let image = Uuid::new_v4();
    let media = Uuid::new_v4();
    let previews =
        json!([{"matched_url":"https://example.org", "title":"Example", "image_upload_id":image}]);
    for (route, body) in [
        (
            "send",
            json!({"body":"https://example.org", "url_previews":previews}),
        ),
        (
            "send-media",
            json!({"upload_id":media,"caption":"https://example.org","url_previews":previews}),
        ),
        ("send", json!({"body":"no card","url_previews":[]})),
    ] {
        let (status, _) = send(
            &app,
            "POST",
            &format!("/v1/accounts/{account}/rooms/!room:localhost/{route}"),
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let calls = sender.calls();
    assert_eq!(calls.len(), 3);
    assert!(
        matches!(&calls[0], Call::SendPreviews { previews, attachment: None, .. } if previews.len()==1 && previews[0].1)
    );
    assert!(matches!(
        &calls[1],
        Call::SendPreviews {
            attachment: Some(_),
            ..
        }
    ));
    assert!(matches!(&calls[2], Call::SendPreviews { previews, .. } if previews.is_empty()));
    assert_eq!(
        uploads.claims(),
        vec![(account, image), (account, image), (account, media)]
    );
    assert_eq!(uploads.completes(), uploads.claims());
    assert!(uploads.releases().is_empty());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn invalid_or_oversized_preview_sends_never_claim_or_dispatch() {
    let sender = Arc::new(StubSender::ok("$must-not-send"));
    let uploads = Arc::new(StubUploads::ok());
    let app = app_with_uploads(store().await, sender.clone(), uploads.clone());
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/link-preview-contract.json")).unwrap();
    for case in fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["status"] != 200)
    {
        let (status, _) = send(
            &app,
            "POST",
            "/v1/accounts/00000000-0000-0000-0000-000000000000/rooms/!room:localhost/send",
            Some(case["request"].clone()),
        )
        .await;
        assert_eq!(
            status.as_u16() as u64,
            case["status"].as_u64().unwrap(),
            "{}",
            case["name"]
        );
    }
    assert!(sender.calls().is_empty());
    assert!(uploads.claims().is_empty());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn failed_preview_send_releases_all_owned_uploads() {
    let sender = Arc::new(StubSender::failing(Outcome::Upstream(
        "response lost".into(),
    )));
    let uploads = Arc::new(StubUploads::ok());
    let app = app_with_uploads(store().await, sender.clone(), uploads.clone());
    let account = Uuid::new_v4();
    let (status, _) = send(&app,"POST",&format!("/v1/accounts/{account}/rooms/!room:localhost/send"),Some(json!({"body":"https://example.org", "url_previews":[{"matched_url":"https://example.org","image_upload_id":Uuid::new_v4()}]}))).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(uploads.releases(), uploads.claims());
    assert!(uploads.completes().is_empty());
    assert_eq!(sender.calls().len(), 1);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn http_cancellation_does_not_abandon_preview_claim_bookkeeping() {
    let gate = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
    let sender = Arc::new(StubSender::ok("$sent").with_preview_gate(gate.clone()));
    let uploads = Arc::new(StubUploads::ok());
    let app = app_with_uploads(store().await, sender.clone(), uploads.clone());
    let pending = tokio::spawn(async move {
        send(&app,"POST","/v1/accounts/00000000-0000-0000-0000-000000000000/rooms/!room:localhost/send",Some(json!({"body":"https://example.org","url_previews":[{"matched_url":"https://example.org","image_upload_id":Uuid::new_v4()}]}))).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.0.notified())
        .await
        .unwrap();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    gate.1.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while uploads.completes().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(uploads.completes(), uploads.claims());
    assert!(uploads.releases().is_empty());
    assert_eq!(sender.calls().len(), 1);
}

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
                shortcode: None,
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
async fn custom_reaction_name_is_validated_and_forwarded() {
    let sender = Arc::new(StubSender::ok("$named:localhost"));
    let app = app(store().await, sender.clone());
    let account_id = Uuid::new_v4();
    let uri = format!(
        "/v1/accounts/{account_id}/rooms/!room:localhost/events/$event:localhost/reactions"
    );
    let (status, _) = send(
        &app,
        "POST",
        &uri,
        Some(json!({
            "key": "mxc://example.org/custom", "shortcode": "\u{00a0}allthethings\u{feff}"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        sender.calls(),
        vec![Call::React {
            account_id,
            room_id: "!room:localhost".into(),
            event_id: "$event:localhost".into(),
            key: "mxc://example.org/custom".into(),
            shortcode: Some("allthethings".into()),
        }]
    );
    for body in [
        json!({"key": "👍", "shortcode": "pretend"}),
        json!({"key": "mxc://example.org/custom", "shortcode": "\u{a0}\u{feff}"}),
        json!({"key": "mxc://example.org/custom", "shortcode": "bad\u{85}name"}),
        json!({"key": "mxc://example.org/custom", "shortcode": "é".repeat(51)}),
    ] {
        assert_eq!(
            send(&app, "POST", &uri, Some(body)).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(sender.calls().len(), 1);
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
async fn send_media_empty_caption_reaches_the_sender_as_none() {
    let store = store().await;
    let sender = Arc::new(StubSender::ok("$media:localhost"));
    let uploads = Arc::new(StubUploads::ok());
    let app = app_with_uploads(store, sender.clone(), uploads.clone());

    let account_id = Uuid::new_v4();
    let (status, _) = send(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/rooms/!room:localhost/send-media"),
        Some(json!({ "upload_id": Uuid::new_v4(), "caption": "" })),
    )
    .await;

    // Matching the edit path: an empty caption is no caption, so the staged
    // filename stays the event body instead of an empty one.
    assert_eq!(status, StatusCode::OK);
    let calls = sender.calls();
    let [Call::SendMedia { caption, .. }] = calls.as_slice() else {
        panic!("expected one media send, got {calls:?}");
    };
    assert_eq!(caption.as_deref(), None);
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
        // An empty caption is no caption, so this is the same request.
        json!({
            "upload_id": Uuid::new_v4(),
            "caption": "",
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

#[tokio::test]
#[ignore = "requires Postgres"]
async fn stickers_validate_and_forward_metadata_and_relations() {
    let sender = Arc::new(StubSender::ok("$sticker"));
    let app = app(store().await, sender.clone());
    let account = Uuid::new_v4();
    let uri = format!("/v1/accounts/{account}/rooms/!stickers:hs/send-sticker");
    for info in [
        json!({}),
        json!({"w":278,"h":185,"mimetype":"image/png","size":129986,"is_animated":false}),
    ] {
        let (status, body) = send(
            &app,
            "POST",
            &uri,
            Some(json!({"body":"Mnt","url":"mxc://hs/sticker","info":info,"thread_root":"$root"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["event_id"], "$sticker");
    }
    assert_eq!(sender.calls().len(), 2);
    let calls = sender.calls();
    let Call::Sticker {
        sticker,
        thread_root,
        ..
    } = &calls[1]
    else {
        panic!("sticker call expected")
    };
    assert_eq!(sticker.info["w"], 278);
    assert_eq!(thread_root.as_deref(), Some("$root"));
    for request in [
        json!({"body":" ","url":"mxc://hs/x"}),
        json!({"body":"x","url":"https://tracker.test/x"}),
        json!({"body":"x","url":"mxc://hs/x","info":{"w":0}}),
        json!({"body":"x","url":"mxc://hs/x","info":{"h":999999}}),
        json!({"body":"x","url":"mxc://hs/x","info":{"mimetype":"text/html"}}),
    ] {
        assert_eq!(
            send(&app, "POST", &uri, Some(request)).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(sender.calls().len(), 2);
}
