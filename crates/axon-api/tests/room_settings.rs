//! Handler tests for the room-settings routes (ADR 0068, M19d): `name`,
//! `topic`, `avatar`, `tags`.
//!
//! Mirrors `tests/membership.rs`: drives the real router with a
//! [`StubRoomSettings`] in place of the SDK gateway, asserting routing,
//! request decoding, the success envelope, and `SendError` → HTTP-status
//! mapping without a homeserver. `set_room_avatar` additionally exercises the
//! claim/complete/release staged-upload flow, mirroring
//! `tests/mutations.rs`'s `send_media` tests.
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-api --test room_settings -- --ignored
//! ```

mod common;

use std::sync::Arc;

use axon_api::{AppState, MediaAttachment, MediaSendKind, RoomSettingsSender, StagedUploadService};
use axon_store::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    OrderedFloat, RoomSettingsCall, RoomSettingsOutcome, StubDeviceList, StubLifecycle,
    StubMediaProxy, StubRoomSettings, StubSender, StubTokenVerifier, StubTrust, StubUploads,
    StubVerification, TEST_TOKEN,
};
use serde_json::{json, Value};
use tower::ServiceExt; // for `oneshot`
use uuid::Uuid;

async fn store() -> Store {
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
    Store::connect(&url, 5).await.expect("connect + migrate")
}

/// Build the router around a given room-settings sender, with a default
/// (always-succeeding) staged-upload service. The live bus, message sender,
/// and lifecycle port are unused here.
fn app(store: Store, room_settings: Arc<dyn RoomSettingsSender>) -> axum::Router {
    app_with_uploads(store, room_settings, Arc::new(StubUploads::ok()))
}

fn app_with_uploads(
    store: Store,
    room_settings: Arc<dyn RoomSettingsSender>,
    uploads: Arc<dyn StagedUploadService>,
) -> axum::Router {
    let (live, _rx) = tokio::sync::broadcast::channel(16);
    let sender = Arc::new(StubSender::ok("$unused:localhost"));
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
        .with_room_settings(room_settings)
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
async fn set_name_routes_to_sender_and_envelope_result() {
    let store = store().await;
    let stub = Arc::new(StubRoomSettings::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";

    let (status, body) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/name"),
        Some(json!({ "name": "Project Chat" })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));
    assert_eq!(
        stub.calls(),
        vec![RoomSettingsCall::SetName {
            account_id,
            room_id: room_id.to_owned(),
            name: "Project Chat".to_owned(),
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn set_name_accepts_an_empty_name_as_clear() {
    let store = store().await;
    let stub = Arc::new(StubRoomSettings::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";

    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/name"),
        Some(json!({ "name": "" })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        stub.calls(),
        vec![RoomSettingsCall::SetName {
            account_id,
            room_id: room_id.to_owned(),
            name: String::new(),
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn set_topic_routes_to_sender_and_envelope_result() {
    let store = store().await;
    let stub = Arc::new(StubRoomSettings::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";

    let (status, body) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/topic"),
        Some(json!({ "topic": "Ship it" })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));
    assert_eq!(
        stub.calls(),
        vec![RoomSettingsCall::SetTopic {
            account_id,
            room_id: room_id.to_owned(),
            topic: "Ship it".to_owned(),
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn set_avatar_claims_sends_and_completes_upload() {
    let store = store().await;
    let stub = Arc::new(StubRoomSettings::ok());
    let uploads = Arc::new(StubUploads::ok());
    let app = app_with_uploads(store, stub.clone(), uploads.clone());

    let account_id = Uuid::new_v4();
    let upload_id = Uuid::new_v4();
    let room_id = "!room:localhost";

    let (status, body) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/avatar"),
        Some(json!({ "upload_id": upload_id })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));
    assert_eq!(uploads.claims(), vec![(account_id, upload_id)]);
    assert_eq!(uploads.completes(), vec![(account_id, upload_id)]);
    assert!(uploads.releases().is_empty());
    assert_eq!(
        stub.calls(),
        vec![RoomSettingsCall::SetAvatar {
            account_id,
            room_id: room_id.to_owned(),
            attachment: MediaAttachment {
                kind: MediaSendKind::Image,
                filename: "photo.png".to_owned(),
                content_type: Some("image/png".to_owned()),
                size_bytes: 3,
                bytes: b"abc".to_vec(),
                width: None,
                height: None,
                duration_ms: None,
                thumbnail: None,
            },
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn set_avatar_releases_upload_after_sender_failure() {
    let store = store().await;
    let stub = Arc::new(StubRoomSettings::failing(RoomSettingsOutcome::Upstream(
        "homeserver".into(),
    )));
    let uploads = Arc::new(StubUploads::ok());
    let app = app_with_uploads(store, stub, uploads.clone());

    let account_id = Uuid::new_v4();
    let upload_id = Uuid::new_v4();
    let (status, body) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/rooms/!room:localhost/avatar"),
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
async fn remove_avatar_routes_to_sender_and_envelope_result() {
    let store = store().await;
    let stub = Arc::new(StubRoomSettings::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";

    let (status, body) = send(
        &app,
        "DELETE",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/avatar"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));
    assert_eq!(
        stub.calls(),
        vec![RoomSettingsCall::RemoveAvatar {
            account_id,
            room_id: room_id.to_owned(),
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn set_tag_routes_to_sender_with_optional_order() {
    let store = store().await;
    let stub = Arc::new(StubRoomSettings::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";
    let uri = format!("/v1/accounts/{account_id}/rooms/{room_id}/tags/m.favourite");

    let (status, body) = send(&app, "PUT", &uri, Some(json!({ "order": 0.5 }))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));

    let (status, _) = send(&app, "PUT", &uri, Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        stub.calls(),
        vec![
            RoomSettingsCall::SetTag {
                account_id,
                room_id: room_id.to_owned(),
                tag: "m.favourite".to_owned(),
                order: Some(OrderedFloat(0.5)),
            },
            RoomSettingsCall::SetTag {
                account_id,
                room_id: room_id.to_owned(),
                tag: "m.favourite".to_owned(),
                order: None,
            },
        ]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn set_tag_routes_a_custom_user_tag() {
    let store = store().await;
    let stub = Arc::new(StubRoomSettings::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";

    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/tags/u.work"),
        Some(json!({})),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        stub.calls(),
        vec![RoomSettingsCall::SetTag {
            account_id,
            room_id: room_id.to_owned(),
            tag: "u.work".to_owned(),
            order: None,
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn remove_tag_routes_to_sender_and_envelope_result() {
    let store = store().await;
    let stub = Arc::new(StubRoomSettings::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";

    let (status, body) = send(
        &app,
        "DELETE",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/tags/m.lowpriority"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));
    assert_eq!(
        stub.calls(),
        vec![RoomSettingsCall::RemoveTag {
            account_id,
            room_id: room_id.to_owned(),
            tag: "m.lowpriority".to_owned(),
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn room_settings_error_maps_to_status() {
    let store = store().await;
    let account_id = Uuid::new_v4();
    let uri = format!("/v1/accounts/{account_id}/rooms/!r:localhost/name");

    let cases = [
        (
            RoomSettingsOutcome::NotFound("x".into()),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            RoomSettingsOutcome::Forbidden("x".into()),
            StatusCode::FORBIDDEN,
            "forbidden",
        ),
        (
            RoomSettingsOutcome::Unavailable("x".into()),
            StatusCode::SERVICE_UNAVAILABLE,
            "service_unavailable",
        ),
        (
            RoomSettingsOutcome::Invalid("x".into()),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            RoomSettingsOutcome::Upstream("x".into()),
            StatusCode::BAD_GATEWAY,
            "bad_gateway",
        ),
    ];

    for (outcome, want_status, want_code) in cases {
        let app = app(store.clone(), Arc::new(StubRoomSettings::failing(outcome)));
        let (status, body) = send(&app, "PUT", &uri, Some(json!({ "name": "x" }))).await;
        assert_eq!(status, want_status);
        assert_eq!(body["error"]["code"], want_code);
    }
}
