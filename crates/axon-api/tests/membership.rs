//! Handler tests for the existing-room membership routes (ADR 0068, M19b):
//! `leave`, `forget`, `invite`, `kick`, `ban`, `unban`.
//!
//! Mirrors `tests/ephemeral.rs`: drives the real router with a
//! [`StubMembership`] in place of the SDK gateway, asserting routing, request
//! decoding, the success envelope, and `SendError` → HTTP-status mapping
//! without a homeserver.
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-api --test membership -- --ignored
//! ```

mod common;

use std::sync::Arc;

use axon_api::{AppState, MembershipSender};
use axon_core::LiveFrame;
use axon_store::{RoomInviteSnapshot, Store};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    MembershipCall, MembershipOutcome, StubDeviceList, StubLifecycle, StubMediaProxy,
    StubMembership, StubSender, StubTokenVerifier, StubTrust, StubVerification, TEST_TOKEN,
};
use serde_json::{json, Value};
use tower::ServiceExt; // for `oneshot`
use uuid::Uuid;

async fn store() -> Store {
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
    Store::connect(&url, 5).await.expect("connect + migrate")
}

/// Build the router around a given membership sender. The live bus, message
/// sender, and lifecycle port are unused here.
fn app(store: Store, membership: Arc<dyn MembershipSender>) -> axum::Router {
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
        .with_membership(membership),
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
async fn leave_routes_to_sender_and_envelope_result() {
    let store = store().await;
    let stub = Arc::new(StubMembership::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";

    let (status, body) = send(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/leave"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));
    assert_eq!(
        stub.calls(),
        vec![MembershipCall::Leave {
            account_id,
            room_id: room_id.to_owned(),
        }]
    );
}

/// A successful leave drops a persisted invite the SDK may no longer have a
/// `Room` for, and fans out `invite.removed` so reconnect is not required.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn leave_drops_pending_invite_and_emits_removed() {
    let store = store().await;
    let pool = store.pool().clone();
    let account_id = store
        .upsert_account(
            &format!("@leave-invite-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .expect("account")
        .account_id;
    let room_id = format!("!leave-inv-{}:localhost", Uuid::new_v4());
    store
        .upsert_room_invite(
            account_id,
            &room_id,
            &RoomInviteSnapshot {
                name: Some("Pending".to_owned()),
                avatar_url: None,
                topic: None,
                canonical_alias: None,
                room_type: None,
                inviter_user_id: "@alice:localhost".to_owned(),
                inviter_display_name: None,
                is_direct: false,
                encrypted: false,
            },
        )
        .await
        .expect("seed invite");

    let stub = Arc::new(StubMembership::ok());
    let (live, mut rx) = tokio::sync::broadcast::channel(16);
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
        .with_membership(stub),
    );

    let (status, body) = send(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/leave"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));

    let remaining = store.list_invites(Some(account_id)).await.expect("list");
    assert!(
        remaining.iter().all(|invite| invite.room_id != room_id),
        "leave must drop the invite row, got {remaining:?}"
    );

    match rx.try_recv().expect("invite.removed") {
        LiveFrame::InviteRemoved(frame) => {
            assert_eq!(frame.account_id, account_id);
            assert_eq!(frame.room_id, room_id);
        }
        other => panic!("unexpected live frame: {other:?}"),
    }

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

/// A failed leave must not delete the invite row or emit `invite.removed`.
/// The inbox stays the reconnect source of truth until the homeserver
/// actually accepts the decline.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn failed_leave_does_not_drop_pending_invite() {
    let store = store().await;
    let pool = store.pool().clone();
    let account_id = store
        .upsert_account(
            &format!("@leave-keep-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .expect("account")
        .account_id;
    let room_id = format!("!leave-keep-{}:localhost", Uuid::new_v4());
    store
        .upsert_room_invite(
            account_id,
            &room_id,
            &RoomInviteSnapshot {
                name: Some("Still pending".to_owned()),
                avatar_url: None,
                topic: None,
                canonical_alias: None,
                room_type: None,
                inviter_user_id: "@alice:localhost".to_owned(),
                inviter_display_name: None,
                is_direct: false,
                encrypted: false,
            },
        )
        .await
        .expect("seed invite");

    let stub = Arc::new(StubMembership::failing(MembershipOutcome::NotFound(
        format!("room not found: {room_id}"),
    )));
    let (live, mut rx) = tokio::sync::broadcast::channel(16);
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
        .with_membership(stub),
    );

    let (status, body) = send(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/leave"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");

    let remaining = store.list_invites(Some(account_id)).await.expect("list");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].room_id, room_id);
    assert!(
        rx.try_recv().is_err(),
        "failed leave must not emit invite.removed"
    );

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

/// A leave the homeserver would not confirm (`M_FORBIDDEN` on the
/// no-local-`Room` fallback) still answers 200 — the request stands — but it
/// is not evidence the invite is gone, so the row and the inbox stay put.
/// Deleting here would silently destroy an invite that is still pending;
/// ADR 0091 puts that reconciliation on the sync watcher, which has a
/// per-room signal to do it with.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn unconfirmed_leave_keeps_pending_invite() {
    let store = store().await;
    let pool = store.pool().clone();
    let account_id = store
        .upsert_account(
            &format!("@leave-unconfirmed-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .expect("account")
        .account_id;
    let room_id = format!("!leave-unconf-{}:localhost", Uuid::new_v4());
    store
        .upsert_room_invite(
            account_id,
            &room_id,
            &RoomInviteSnapshot {
                name: Some("Unconfirmed".to_owned()),
                avatar_url: None,
                topic: None,
                canonical_alias: None,
                room_type: None,
                inviter_user_id: "@alice:localhost".to_owned(),
                inviter_display_name: None,
                is_direct: false,
                encrypted: false,
            },
        )
        .await
        .expect("seed invite");

    let stub = Arc::new(StubMembership::ok_unconfirmed());
    let (live, mut rx) = tokio::sync::broadcast::channel(16);
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
        .with_membership(stub),
    );

    let (status, body) = send(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/leave"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));

    let remaining = store.list_invites(Some(account_id)).await.expect("list");
    assert_eq!(
        remaining.len(),
        1,
        "an unconfirmed leave must not drop the invite row, got {remaining:?}"
    );
    assert_eq!(remaining[0].room_id, room_id);
    assert!(
        rx.try_recv().is_err(),
        "an unconfirmed leave must not emit invite.removed"
    );

    sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn forget_routes_to_sender_and_envelope_result() {
    let store = store().await;
    let stub = Arc::new(StubMembership::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";

    let (status, body) = send(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/forget"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));
    assert_eq!(
        stub.calls(),
        vec![MembershipCall::Forget {
            account_id,
            room_id: room_id.to_owned(),
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn invite_routes_to_sender_and_envelope_result() {
    let store = store().await;
    let stub = Arc::new(StubMembership::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";
    let user_id = "@bob:localhost";

    let (status, body) = send(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/invite"),
        Some(json!({ "user_id": user_id })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));
    assert_eq!(
        stub.calls(),
        vec![MembershipCall::Invite {
            account_id,
            room_id: room_id.to_owned(),
            user_id: user_id.to_owned(),
        }]
    );
}

/// Shared body for the three `{user_id, reason?}`-shaped routes: asserts both
/// the with-reason and without-reason request decode the same way and route
/// to the matching [`MembershipCall`] variant.
async fn assert_member_action_routes(
    store: &Store,
    verb: &str,
    expect_call: impl Fn(Uuid, String, String, Option<String>) -> MembershipCall,
) {
    let stub = Arc::new(StubMembership::ok());
    let app = app(store.clone(), stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";
    let user_id = "@bob:localhost";
    let uri = format!("/v1/accounts/{account_id}/rooms/{room_id}/{verb}");

    let (status, body) = send(
        &app,
        "POST",
        &uri,
        Some(json!({ "user_id": user_id, "reason": "spamming" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));

    let (status, _) = send(&app, "POST", &uri, Some(json!({ "user_id": user_id }))).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        stub.calls(),
        vec![
            expect_call(
                account_id,
                room_id.to_owned(),
                user_id.to_owned(),
                Some("spamming".to_owned()),
            ),
            expect_call(account_id, room_id.to_owned(), user_id.to_owned(), None),
        ]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn kick_routes_to_sender_with_optional_reason() {
    let store = store().await;
    assert_member_action_routes(&store, "kick", |account_id, room_id, user_id, reason| {
        MembershipCall::Kick {
            account_id,
            room_id,
            user_id,
            reason,
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn ban_routes_to_sender_with_optional_reason() {
    let store = store().await;
    assert_member_action_routes(&store, "ban", |account_id, room_id, user_id, reason| {
        MembershipCall::Ban {
            account_id,
            room_id,
            user_id,
            reason,
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn unban_routes_to_sender_with_optional_reason() {
    let store = store().await;
    assert_member_action_routes(&store, "unban", |account_id, room_id, user_id, reason| {
        MembershipCall::Unban {
            account_id,
            room_id,
            user_id,
            reason,
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn membership_error_maps_to_status() {
    let store = store().await;
    let account_id = Uuid::new_v4();
    let uri = format!("/v1/accounts/{account_id}/rooms/!r:localhost/leave");

    let cases = [
        (
            MembershipOutcome::NotFound("x".into()),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            MembershipOutcome::Forbidden("x".into()),
            StatusCode::FORBIDDEN,
            "forbidden",
        ),
        (
            MembershipOutcome::Unavailable("x".into()),
            StatusCode::SERVICE_UNAVAILABLE,
            "service_unavailable",
        ),
        (
            MembershipOutcome::Invalid("x".into()),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            MembershipOutcome::Upstream("x".into()),
            StatusCode::BAD_GATEWAY,
            "bad_gateway",
        ),
    ];

    for (outcome, want_status, want_code) in cases {
        let app = app(store.clone(), Arc::new(StubMembership::failing(outcome)));
        let (status, body) = send(&app, "POST", &uri, None).await;
        assert_eq!(status, want_status);
        assert_eq!(body["error"]["code"], want_code);
    }
}
