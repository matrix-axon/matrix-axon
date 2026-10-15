//! `PUT` and `DELETE /v1/push/registrations`, and the push flags on
//! `GET /v1/status` (ADR 0113).
//!
//! Needs a disposable database. The tests insert credentials and delete the
//! rows they created.
//!
//! ```sh
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_push_registrations \
//!   cargo test -p axon-api --test push -- --ignored --test-threads=1
//! ```

mod common;

use std::sync::Arc;

use axon_api::{AppState, StoreTokenVerifier};
use axon_core::{LoadedApns, PushMaterial};
use axon_store::{OAuthSession, Store};
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::{Duration, Utc};
use common::{
    StubDeviceList, StubLifecycle, StubMediaProxy, StubSender, StubTrust, StubVerification,
};
use serde_json::{json, Value};
use sqlx_core::row::Row;
use tower::ServiceExt;
use uuid::Uuid;

async fn store() -> Store {
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
    Store::connect(&url, 5).await.expect("connect + migrate")
}

fn app(store: Store, push: PushMaterial) -> axum::Router {
    let (live, _rx) = tokio::sync::broadcast::channel(16);
    let state = AppState::new(
        store.clone(),
        live,
        Arc::new(StubSender::ok("$unused:localhost")),
        Arc::new(StubLifecycle::ok(Uuid::nil())),
        Arc::new(StubVerification::ok("$unused-flow")),
        Arc::new(StubTrust::ok()),
        Arc::new(StubDeviceList::ok()),
        Arc::new(StoreTokenVerifier::new(store)),
        Arc::new(StubMediaProxy),
        None,
    )
    .with_push(push);
    axon_api::router(state)
}

async fn call(
    app: &axum::Router,
    method: Method,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(bearer) = bearer {
        request = request.header("authorization", format!("Bearer {bearer}"));
    }
    let request = if let Some(body) = body {
        request
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    } else {
        request.body(Body::empty()).unwrap()
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let parsed = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, parsed)
}

fn code(body: &Value) -> Option<&str> {
    body["error"]["code"].as_str()
}

fn apns_only() -> PushMaterial {
    PushMaterial::new(
        Some(LoadedApns {
            team_id: "TEAM".into(),
            key_id: "KEY".into(),
            bundle_id: "org.matrixaxon.axon".into(),
            private_key: b"test-key".to_vec(),
        }),
        None,
    )
}

async fn registration_count(store: &Store) -> i64 {
    sqlx_core::query::query("SELECT count(*) AS n FROM push_registrations")
        .fetch_one(store.pool())
        .await
        .expect("count")
        .try_get("n")
        .expect("n")
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn registration_routes_reject_a_bad_body_and_upsert_without_echoing_the_token() {
    let store = store().await;
    let router = app(store.clone(), apns_only());
    let device = format!("device-token-{}", Uuid::new_v4());

    let (status, body) = call(
        &router,
        Method::PUT,
        "/v1/push/registrations",
        None,
        Some(json!({"provider": "fcm", "token": &device, "disclosure": "activity"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&body), Some("unauthorized"));

    let cli = store.issue_token("push-cli").await.expect("cli token");
    let before = registration_count(&store).await;

    for (payload, expected) in [
        (
            json!({"provider": "webpush", "token": &device, "disclosure": "activity"}),
            "push_provider_unsupported",
        ),
        (
            json!({"provider": "fcm", "token": "", "disclosure": "activity"}),
            "bad_request",
        ),
        (
            json!({"provider": "fcm", "token": "x".repeat(4097), "disclosure": "activity"}),
            "bad_request",
        ),
        (
            json!({"provider": "fcm", "token": &device, "disclosure": "everything"}),
            "bad_request",
        ),
        (json!({"provider": "fcm", "token": &device}), "bad_request"),
        (
            json!({"provider": "apns", "token": &device, "disclosure": "activity"}),
            "push_environment_invalid",
        ),
        (
            json!({
                "provider": "apns",
                "token": &device,
                "disclosure": "activity",
                "environment": ""
            }),
            "push_environment_invalid",
        ),
        (
            json!({
                "provider": "apns",
                "token": &device,
                "disclosure": "activity",
                "environment": "staging"
            }),
            "push_environment_invalid",
        ),
        (
            json!({
                "provider": "fcm",
                "token": &device,
                "disclosure": "activity",
                "environment": "production"
            }),
            "push_environment_invalid",
        ),
    ] {
        let (status, body) = call(
            &router,
            Method::PUT,
            "/v1/push/registrations",
            Some(&cli.token),
            Some(payload),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(code(&body), Some(expected), "{body}");
    }
    assert_eq!(registration_count(&store).await, before);

    let (status, body) = call(
        &router,
        Method::PUT,
        "/v1/push/registrations",
        Some(&cli.token),
        Some(json!({
            "provider": "fcm",
            "token": &device,
            "disclosure": "activity"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(body["data"]["provider"], "fcm");
    assert_eq!(body["data"]["disclosure"], "activity");
    assert!(body["data"]["environment"].is_null());
    assert!(!body.to_string().contains(&device));

    let (status, again) = call(
        &router,
        Method::PUT,
        "/v1/push/registrations",
        Some(&cli.token),
        Some(json!({
            "provider": "fcm",
            "token": &device,
            "disclosure": "preview"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["data"]["id"], id);
    assert_eq!(again["data"]["disclosure"], "preview");

    let other = store.issue_token("push-other").await.expect("other token");
    let (status, rehomed) = call(
        &router,
        Method::PUT,
        "/v1/push/registrations",
        Some(&other.token),
        Some(json!({
            "provider": "apns",
            "token": &device,
            "disclosure": "sender_and_room",
            "environment": "sandbox"
        })),
    )
    .await;
    // A different provider is a different row. Re-home is the same provider.
    assert_eq!(status, StatusCode::OK, "{rehomed}");
    assert_ne!(rehomed["data"]["id"], id);
    assert_eq!(rehomed["data"]["environment"], "sandbox");

    let (status, rehomed) = call(
        &router,
        Method::PUT,
        "/v1/push/registrations",
        Some(&other.token),
        Some(json!({
            "provider": "fcm",
            "token": &device,
            "disclosure": "activity"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rehomed}");
    assert_eq!(rehomed["data"]["id"], id);
    let owner: Uuid =
        sqlx_core::query::query("SELECT session_id FROM push_registrations WHERE id = $1")
            .bind(Uuid::parse_str(&id).unwrap())
            .fetch_one(store.pool())
            .await
            .expect("row")
            .try_get("session_id")
            .expect("session");
    assert_eq!(owner, other.id);

    let (status, _) = call(
        &router,
        Method::DELETE,
        &format!("/v1/push/registrations/{id}"),
        Some(&cli.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let still: bool = sqlx_core::query::query(
        "SELECT EXISTS (SELECT 1 FROM push_registrations WHERE id = $1) AS present",
    )
    .bind(Uuid::parse_str(&id).unwrap())
    .fetch_one(store.pool())
    .await
    .expect("still")
    .try_get("present")
    .expect("present");
    assert!(still, "another session's delete is a no-op");

    let (status, _) = call(
        &router,
        Method::DELETE,
        &format!("/v1/push/registrations/{id}"),
        Some(&other.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &router,
        Method::DELETE,
        &format!("/v1/push/registrations/{}", Uuid::new_v4()),
        Some(&other.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, status_body) =
        call(&router, Method::GET, "/v1/status", Some(&cli.token), None).await;
    assert_eq!(status, StatusCode::OK, "{status_body}");
    assert_eq!(status_body["data"]["push"]["apns"], true);
    assert_eq!(status_body["data"]["push"]["fcm"], false);

    let plain = app(store.clone(), PushMaterial::unconfigured());
    let (status, status_body) =
        call(&plain, Method::GET, "/v1/status", Some(&cli.token), None).await;
    assert_eq!(status, StatusCode::OK, "{status_body}");
    assert_eq!(status_body["data"]["push"]["apns"], false);
    assert_eq!(status_body["data"]["push"]["fcm"], false);

    sqlx_core::query::query(
        "DELETE FROM push_registrations WHERE session_id = $1 OR session_id = $2",
    )
    .bind(cli.id)
    .bind(other.id)
    .execute(store.pool())
    .await
    .expect("delete registrations");
    sqlx_core::query::query("DELETE FROM tokens WHERE id = $1 OR id = $2")
        .bind(cli.id)
        .bind(other.id)
        .execute(store.pool())
        .await
        .expect("delete tokens");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn an_expiring_token_needs_a_live_refresh_token_on_its_own_session() {
    let store = store().await;
    let router = app(store.clone(), PushMaterial::unconfigured());
    let identity = store
        .bind_identity("google", &format!("sub-{}", Uuid::new_v4()), None)
        .await
        .expect("bind");
    let session_id = Uuid::new_v4();
    let other_session = Uuid::new_v4();
    let future = Utc::now() + Duration::hours(1);
    let past = Utc::now() - Duration::minutes(1);
    let session = OAuthSession {
        session_id,
        authenticated_at: Some(Utc::now()),
    };
    let access = store
        .issue_oauth_token("oauth", future, "google", identity.id, "client", session)
        .await
        .expect("access");
    let device = format!("device-{}", Uuid::new_v4());
    let body = json!({
        "provider": "fcm",
        "token": device,
        "disclosure": "activity"
    });

    let (status, response) = call(
        &router,
        Method::PUT,
        "/v1/push/registrations",
        Some(&access.token),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    assert_eq!(code(&response), Some("push_session_refresh_required"));

    let refresh_id = store
        .issue_refresh_token(
            &format!("hash-{}", Uuid::new_v4()),
            identity.id,
            "client",
            future,
            OAuthSession {
                session_id: other_session,
                authenticated_at: Some(Utc::now()),
            },
        )
        .await
        .expect("other device refresh");
    let (status, response) = call(
        &router,
        Method::PUT,
        "/v1/push/registrations",
        Some(&access.token),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    assert_eq!(
        code(&response),
        Some("push_session_refresh_required"),
        "another device's refresh token is not this session"
    );

    sqlx_core::query::query(
        "UPDATE oauth_refresh_tokens SET session_id = $2, revoked_at = now() WHERE id = $1",
    )
    .bind(refresh_id)
    .bind(session_id)
    .execute(store.pool())
    .await
    .expect("revoke refresh");
    let (status, response) = call(
        &router,
        Method::PUT,
        "/v1/push/registrations",
        Some(&access.token),
        Some(body.clone()),
    )
    .await;
    assert_eq!(
        code(&response),
        Some("push_session_refresh_required"),
        "{response}"
    );
    assert_eq!(status, StatusCode::BAD_REQUEST);

    sqlx_core::query::query(
        "UPDATE oauth_refresh_tokens SET revoked_at = NULL, expires_at = $2 WHERE id = $1",
    )
    .bind(refresh_id)
    .bind(past)
    .execute(store.pool())
    .await
    .expect("expire refresh");
    let (status, response) = call(
        &router,
        Method::PUT,
        "/v1/push/registrations",
        Some(&access.token),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    assert_eq!(code(&response), Some("push_session_refresh_required"));

    sqlx_core::query::query("UPDATE oauth_refresh_tokens SET expires_at = $2 WHERE id = $1")
        .bind(refresh_id)
        .bind(future)
        .execute(store.pool())
        .await
        .expect("revive refresh");
    let (status, response) = call(
        &router,
        Method::PUT,
        "/v1/push/registrations",
        Some(&access.token),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let id = response["data"]["id"].as_str().unwrap();
    assert!(!response.to_string().contains(&device));

    sqlx_core::query::query("DELETE FROM push_registrations WHERE id = $1")
        .bind(Uuid::parse_str(id).unwrap())
        .execute(store.pool())
        .await
        .expect("delete registration");
    sqlx_core::query::query("DELETE FROM oauth_refresh_tokens WHERE oauth_identity_id = $1")
        .bind(identity.id)
        .execute(store.pool())
        .await
        .expect("delete refresh");
    sqlx_core::query::query("DELETE FROM tokens WHERE id = $1")
        .bind(access.id)
        .execute(store.pool())
        .await
        .expect("delete access");
    sqlx_core::query::query("DELETE FROM oauth_identities WHERE id = $1")
        .bind(identity.id)
        .execute(store.pool())
        .await
        .expect("delete identity");
}
