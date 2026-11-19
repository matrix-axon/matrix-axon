//! Handler tests for the power-levels routes (ADR 0068, M19e): role
//! thresholds and per-user levels, merged into one `m.room.power_levels`
//! write, plus the resolved-levels read.
//!
//! Mirrors `tests/room_settings.rs`: drives the real router with a
//! [`StubPowerLevels`] in place of the SDK gateway, asserting routing,
//! request decoding, the success envelope, and `SendError` → HTTP-status
//! mapping without a homeserver. The self-demotion guardrail itself is a
//! pure function in `axon-sync`'s gateway, covered by
//! `gateway::tests::check_self_demotion_guardrail_*` there — this suite
//! only exercises the handler layer around the stubbed port.
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-api --test power_levels -- --ignored
//! ```

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use axon_api::{AppState, PowerLevelsSender};
use axon_core::{PowerLevelChanges, ResolvedPowerLevels, ResolvedUserLevel};
use axon_store::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    PowerLevelsCall, PowerLevelsOutcome, StubDeviceList, StubLifecycle, StubMediaProxy,
    StubPowerLevels, StubSender, StubTokenVerifier, StubTrust, StubVerification, TEST_TOKEN,
};
use serde_json::{json, Value};
use tower::ServiceExt; // for `oneshot`
use uuid::Uuid;

async fn store() -> Store {
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
    Store::connect(&url, 5).await.expect("connect + migrate")
}

fn app(store: Store, power_levels: Arc<dyn PowerLevelsSender>) -> axum::Router {
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
        .with_power_levels(power_levels),
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
async fn set_power_levels_routes_role_thresholds_to_sender_and_envelope_result() {
    let store = store().await;
    let stub = Arc::new(StubPowerLevels::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";

    let (status, body) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/power_levels"),
        Some(json!({ "ban": 80, "state_default": 60 })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({}));
    assert_eq!(
        stub.calls(),
        vec![PowerLevelsCall::SetPowerLevels {
            account_id,
            room_id: room_id.to_owned(),
            changes: PowerLevelChanges {
                ban: Some(80),
                state_default: Some(60),
                ..Default::default()
            },
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn set_power_levels_routes_user_levels_and_ack_flag() {
    let store = store().await;
    let stub = Arc::new(StubPowerLevels::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";

    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/power_levels"),
        Some(json!({
            "users": { "@bob:example.org": 50 },
            "acknowledge_self_demotion": true,
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let mut users = HashMap::new();
    users.insert("@bob:example.org".to_owned(), 50);
    assert_eq!(
        stub.calls(),
        vec![PowerLevelsCall::SetPowerLevels {
            account_id,
            room_id: room_id.to_owned(),
            changes: PowerLevelChanges {
                users,
                acknowledge_self_demotion: true,
                ..Default::default()
            },
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn set_power_levels_defaults_an_empty_body_to_no_changes() {
    let store = store().await;
    let stub = Arc::new(StubPowerLevels::ok());
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";

    let (status, _) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/power_levels"),
        Some(json!({})),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        stub.calls(),
        vec![PowerLevelsCall::SetPowerLevels {
            account_id,
            room_id: room_id.to_owned(),
            changes: PowerLevelChanges::default(),
        }]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn set_power_levels_maps_a_guardrail_rejection_to_bad_request() {
    let store = store().await;
    let stub = Arc::new(StubPowerLevels::failing(PowerLevelsOutcome::Invalid(
        "this change would drop your own power level".into(),
    )));
    let app = app(store, stub);

    let account_id = Uuid::new_v4();
    let (status, body) = send(
        &app,
        "PUT",
        &format!("/v1/accounts/{account_id}/rooms/!r:localhost/power_levels"),
        Some(json!({ "users_default": -1 })),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn get_power_levels_routes_to_sender_and_returns_resolved_levels() {
    let store = store().await;
    let mut users = HashMap::new();
    users.insert("@alice:example.org".to_owned(), 100);
    let resolved = ResolvedPowerLevels {
        ban: 50,
        invite: 0,
        kick: 50,
        redact: 50,
        events_default: 0,
        state_default: 50,
        users_default: 0,
        users,
        privileged_creators: Vec::new(),
        own_level: ResolvedUserLevel::Int(100),
    };
    let stub = Arc::new(StubPowerLevels::with_read(resolved));
    let app = app(store, stub.clone());

    let account_id = Uuid::new_v4();
    let room_id = "!room:localhost";

    let (status, body) = send(
        &app,
        "GET",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/power_levels"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["data"],
        json!({
            "ban": 50,
            "invite": 0,
            "kick": 50,
            "redact": 50,
            "events_default": 0,
            "state_default": 50,
            "users_default": 0,
            "users": { "@alice:example.org": 100 },
            "privileged_creators": [],
            "own_level": 100,
        })
    );
    assert_eq!(
        stub.calls(),
        vec![PowerLevelsCall::GetPowerLevels {
            account_id,
            room_id: room_id.to_owned(),
        }]
    );
}

/// Issue 324: a room-version-12 creator is absent from `users`, so the read
/// must say who the creators are and that the caller is one, as the string
/// `"infinite"` rather than a number a client could mistake for a real level.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn get_power_levels_reports_a_v12_creator_as_infinite() {
    let store = store().await;
    let resolved = ResolvedPowerLevels {
        ban: 50,
        invite: 0,
        kick: 50,
        redact: 50,
        events_default: 0,
        state_default: 50,
        users_default: 0,
        users: HashMap::new(),
        privileged_creators: vec!["@alice:example.org".to_owned()],
        own_level: ResolvedUserLevel::Infinite,
    };
    let app = app(store, Arc::new(StubPowerLevels::with_read(resolved)));

    let account_id = Uuid::new_v4();
    let (status, body) = send(
        &app,
        "GET",
        &format!("/v1/accounts/{account_id}/rooms/!room:localhost/power_levels"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["users"], json!({}));
    assert_eq!(
        body["data"]["privileged_creators"],
        json!(["@alice:example.org"])
    );
    assert_eq!(body["data"]["own_level"], json!("infinite"));
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn power_levels_error_maps_to_status() {
    let store = store().await;
    let account_id = Uuid::new_v4();
    let uri = format!("/v1/accounts/{account_id}/rooms/!r:localhost/power_levels");

    let cases = [
        (
            PowerLevelsOutcome::NotFound("x".into()),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            PowerLevelsOutcome::Forbidden("x".into()),
            StatusCode::FORBIDDEN,
            "forbidden",
        ),
        (
            PowerLevelsOutcome::Unavailable("x".into()),
            StatusCode::SERVICE_UNAVAILABLE,
            "service_unavailable",
        ),
        (
            PowerLevelsOutcome::Invalid("x".into()),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            PowerLevelsOutcome::Upstream("x".into()),
            StatusCode::BAD_GATEWAY,
            "bad_gateway",
        ),
    ];

    for (outcome, expected_status, expected_code) in cases {
        let stub = Arc::new(StubPowerLevels::failing(outcome));
        let app = app(store.clone(), stub);

        let (status, body) = send(&app, "PUT", &uri, Some(json!({}))).await;
        assert_eq!(status, expected_status);
        assert_eq!(body["error"]["code"], expected_code);
    }
}
