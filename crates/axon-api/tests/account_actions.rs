//! Handler tests for the account-actions routes (ADR 0068, M19f): profile
//! display-name/avatar, reading another user's profile, this account's
//! ignore list, and public-room directory search.
//!
//! Mirrors `tests/power_levels.rs`: drives the real router with a
//! [`StubAccountActions`] in place of the SDK gateway, asserting routing,
//! request decoding, the success envelope, and `SendError` → HTTP-status
//! mapping without a homeserver. `set_account_avatar` additionally exercises
//! the claim/complete/release staged-upload flow, mirroring
//! `tests/room_settings.rs`'s `set_avatar` tests.
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-api --test account_actions -- --ignored
//! ```

mod common;

use std::sync::Arc;

use axon_api::{
    AccountActionsSender, AppState, MediaAttachment, MediaSendKind, StagedUploadService,
};
use axon_core::{MatrixProfile, PublicRoomSummary, PublicRoomsPage, PublicRoomsQuery};
use axon_store::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    AccountActionsCall, AccountActionsOutcome, StubAccountActions, StubDeviceList, StubLifecycle,
    StubMediaProxy, StubSender, StubTokenVerifier, StubTrust, StubUploads, StubVerification,
    TEST_TOKEN,
};
use serde_json::{json, Value};
use tower::ServiceExt; // for `oneshot`
use uuid::Uuid;

async fn store() -> Store {
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
    Store::connect(&url, 5).await.expect("connect + migrate")
}

fn app(store: Store, account_actions: Arc<dyn AccountActionsSender>) -> axum::Router {
    app_with_uploads(store, account_actions, Arc::new(StubUploads::ok()))
}

fn app_with_uploads(
    store: Store,
    account_actions: Arc<dyn AccountActionsSender>,
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
        .with_account_actions(account_actions)
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
async fn set_display_name_routes_to_sender_and_envelope_result() {
    let store = store().await;
    let stub = Arc::new(StubAccountActions::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();

    let (status, body) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/profile/display_name"),
        Some(json!({ "display_name": "Alice" })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));
    assert_eq!(
        stub.calls(),
        vec![AccountActionsCall::SetDisplayName {
            account_id,
            display_name: "Alice".to_owned(),
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn set_display_name_accepts_an_empty_name_as_clear() {
    let store = store().await;
    let stub = Arc::new(StubAccountActions::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();

    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/profile/display_name"),
        Some(json!({ "display_name": "" })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        stub.calls(),
        vec![AccountActionsCall::SetDisplayName {
            account_id,
            display_name: String::new(),
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn set_account_avatar_claims_sends_and_completes_upload() {
    let store = store().await;
    let stub = Arc::new(StubAccountActions::ok());
    let uploads = Arc::new(StubUploads::ok());
    let app = app_with_uploads(store, stub.clone(), uploads.clone());

    let account_id = Uuid::new_v4();
    let upload_id = Uuid::new_v4();

    let (status, body) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/profile/avatar"),
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
        vec![AccountActionsCall::SetAvatar {
            account_id,
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
async fn set_account_avatar_releases_upload_after_sender_failure() {
    let store = store().await;
    let stub = Arc::new(StubAccountActions::failing(
        AccountActionsOutcome::Upstream("homeserver".into()),
    ));
    let uploads = Arc::new(StubUploads::ok());
    let app = app_with_uploads(store, stub, uploads.clone());

    let account_id = Uuid::new_v4();
    let upload_id = Uuid::new_v4();
    let (status, body) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/profile/avatar"),
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
async fn remove_account_avatar_routes_to_sender_and_envelope_result() {
    let store = store().await;
    let stub = Arc::new(StubAccountActions::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();

    let (status, body) = send(
        &app,
        "DELETE",
        &format!("/v1/accounts/{account_id}/profile/avatar"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));
    assert_eq!(
        stub.calls(),
        vec![AccountActionsCall::RemoveAvatar { account_id }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn get_user_profile_routes_to_sender_and_returns_profile() {
    let store = store().await;
    let profile = MatrixProfile {
        display_name: Some("Bob".to_owned()),
        avatar_url: Some("mxc://example.org/abc".to_owned()),
    };
    let stub = Arc::new(StubAccountActions::with_profile(profile));
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let user_id = "@bob:example.org";

    let (status, body) = send(
        &app,
        "GET",
        &format!("/v1/accounts/{account_id}/users/{user_id}/profile"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["data"],
        json!({
            "user_id": user_id,
            "display_name": "Bob",
            "avatar_url": "mxc://example.org/abc",
        })
    );
    assert_eq!(
        stub.calls(),
        vec![AccountActionsCall::UserProfile {
            account_id,
            user_id: user_id.to_owned(),
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn ignore_user_routes_to_sender_and_envelope_result() {
    let store = store().await;
    let stub = Arc::new(StubAccountActions::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let user_id = "@spammer:example.org";

    let (status, body) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/users/{user_id}/ignore"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));
    assert_eq!(
        stub.calls(),
        vec![AccountActionsCall::IgnoreUser {
            account_id,
            user_id: user_id.to_owned(),
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn unignore_user_routes_to_sender_and_envelope_result() {
    let store = store().await;
    let stub = Arc::new(StubAccountActions::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let user_id = "@spammer:example.org";

    let (status, body) = send(
        &app,
        "DELETE",
        &format!("/v1/accounts/{account_id}/users/{user_id}/ignore"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));
    assert_eq!(
        stub.calls(),
        vec![AccountActionsCall::UnignoreUser {
            account_id,
            user_id: user_id.to_owned(),
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn ignore_user_maps_invalid_to_bad_request() {
    let store = store().await;
    let stub = Arc::new(StubAccountActions::failing(AccountActionsOutcome::Invalid(
        "cannot ignore this account's own user id".into(),
    )));
    let app = app(store, stub);

    let account_id = Uuid::new_v4();
    let (status, body) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/users/@me:example.org/ignore"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn search_public_rooms_routes_query_params_and_returns_page() {
    let store = store().await;
    let page = PublicRoomsPage {
        chunk: vec![PublicRoomSummary {
            room_id: "!pub:example.org".to_owned(),
            canonical_alias: Some("#pub:example.org".to_owned()),
            name: Some("Public Room".to_owned()),
            topic: Some("Everyone welcome".to_owned()),
            avatar_url: None,
            num_joined_members: 42,
            world_readable: true,
            guest_can_join: false,
            join_rule: "public".to_owned(),
            room_type: None,
        }],
        next_batch: Some("batch2".to_owned()),
        prev_batch: None,
        total_room_count_estimate: Some(100),
    };
    let stub = Arc::new(StubAccountActions::with_public_rooms(page));
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let (status, body) = send(
        &app,
        "GET",
        &format!(
            "/v1/accounts/{account_id}/directory/public_rooms?search_term=rust&limit=10&since=batch1"
        ),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["chunk"][0]["room_id"], "!pub:example.org");
    assert_eq!(body["data"]["next_batch"], "batch2");
    assert_eq!(body["data"]["total_room_count_estimate"], 100);
    assert_eq!(
        stub.calls(),
        vec![AccountActionsCall::PublicRooms {
            account_id,
            query: PublicRoomsQuery {
                server: None,
                search_term: Some("rust".to_owned()),
                limit: Some(10),
                since: Some("batch1".to_owned()),
            },
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn search_public_rooms_clamps_an_oversized_limit() {
    let store = store().await;
    let stub = Arc::new(StubAccountActions::with_public_rooms(
        PublicRoomsPage::default(),
    ));
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let (status, _) = send(
        &app,
        "GET",
        &format!("/v1/accounts/{account_id}/directory/public_rooms?limit=4000000000"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        stub.calls(),
        vec![AccountActionsCall::PublicRooms {
            account_id,
            query: PublicRoomsQuery {
                server: None,
                search_term: None,
                limit: Some(200),
                since: None,
            },
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn search_public_rooms_leaves_an_omitted_limit_unclamped() {
    let store = store().await;
    let stub = Arc::new(StubAccountActions::with_public_rooms(
        PublicRoomsPage::default(),
    ));
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let (status, _) = send(
        &app,
        "GET",
        &format!("/v1/accounts/{account_id}/directory/public_rooms"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        stub.calls(),
        vec![AccountActionsCall::PublicRooms {
            account_id,
            query: PublicRoomsQuery {
                server: None,
                search_term: None,
                limit: None,
                since: None,
            },
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn account_actions_error_maps_to_status() {
    let store = store().await;
    let account_id = Uuid::new_v4();
    let uri = format!("/v1/accounts/{account_id}/profile/display_name");

    let cases = [
        (
            AccountActionsOutcome::NotFound("x".into()),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            AccountActionsOutcome::Forbidden("x".into()),
            StatusCode::FORBIDDEN,
            "forbidden",
        ),
        (
            AccountActionsOutcome::Unavailable("x".into()),
            StatusCode::SERVICE_UNAVAILABLE,
            "service_unavailable",
        ),
        (
            AccountActionsOutcome::Invalid("x".into()),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            AccountActionsOutcome::Upstream("x".into()),
            StatusCode::BAD_GATEWAY,
            "bad_gateway",
        ),
    ];

    for (outcome, expected_status, expected_code) in cases {
        let stub = Arc::new(StubAccountActions::failing(outcome));
        let app = app(store.clone(), stub);

        let (status, body) = send(&app, "PUT", &uri, Some(json!({ "display_name": "x" }))).await;
        assert_eq!(status, expected_status);
        assert_eq!(body["error"]["code"], expected_code);
    }
}
