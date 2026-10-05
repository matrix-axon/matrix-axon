//! DB-gated tests for the management API (ADR 0109): the operator's switch,
//! the step-up rule for credential changes, and listing and unbinding sign-in
//! identities.
//!
//! Needs a disposable database and is `#[ignore]`d by default. The lockout
//! guard counts every credential on the instance, so these tests clear the
//! credential tables: never point them at a database you care about.
//!
//! ```sh
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test \
//!   cargo test -p axon-api --test management -- --ignored --test-threads=1
//! ```

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use axon_api::{AppState, OAuthRuntime, OidcProvider, StoreTokenVerifier, RECENT_SIGN_IN_WINDOW};
use axon_core::{OauthClientConfig, OauthConfig};
use axon_store::Store;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use common::oidc::TestOidcProvider;
use common::{
    StubDeviceList, StubLifecycle, StubMediaProxy, StubSender, StubTrust, StubVerification,
};
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

const CLIENT_ID: &str = "test-client";
const IDENTITIES: &str = "/v1/management/oauth/identities";
const NATIVE_CHALLENGE: &str = "/v1/oauth/apple/native/challenge";

async fn store() -> Store {
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
    Store::connect(&url, 5).await.expect("connect + migrate")
}

async fn clear_credentials(store: &Store) {
    for sql in [
        "DELETE FROM oauth_native_challenges",
        "DELETE FROM oauth_authorization_requests",
        "DELETE FROM oauth_bind_requests",
        "DELETE FROM oauth_refresh_tokens",
        "DELETE FROM tokens",
        "DELETE FROM oauth_identities",
    ] {
        sqlx_core::query::query(sql)
            .execute(store.pool())
            .await
            .unwrap();
    }
}

/// Which upstream providers the test server has switched on.
#[derive(Clone, Copy)]
enum Providers {
    /// OAuth is off entirely.
    None,
    /// The browser flow for each named provider.
    Browser(&'static [&'static str]),
    /// Native Apple only, as an iOS-only deployment runs it.
    NativeApple,
}

fn app(store: Store, management: bool, providers: Providers) -> axum::Router {
    app_and_native_provider(store, management, providers).0
}

/// The router, plus the fake behind native Apple (present for
/// [`Providers::NativeApple`]) so a test can sign identity tokens for it.
fn app_and_native_provider(
    store: Store,
    management: bool,
    providers: Providers,
) -> (axum::Router, Option<Arc<TestOidcProvider>>) {
    let (live, _rx) = tokio::sync::broadcast::channel(16);
    let mut state = AppState::new(
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
    .with_management(management);

    let fake = |name| Arc::new(TestOidcProvider::new(name, "https://idp.test/", "audience"));
    let mut native = None;
    let config = OauthConfig {
        enabled: true,
        external_base_url: Some("http://axon.test".to_owned()),
        access_token_ttl_secs: 3600,
        refresh_token_ttl_secs: 2_592_000,
        clients: vec![OauthClientConfig {
            client_id: CLIENT_ID.to_owned(),
            redirect_uris: vec!["https://client.test/callback".to_owned()],
        }],
        providers: axon_core::OauthProvidersConfig::default(),
    };
    match providers {
        Providers::None => {}
        Providers::Browser(names) => {
            let map: HashMap<&'static str, Arc<dyn OidcProvider>> = names
                .iter()
                .map(|name| (*name, fake(name) as Arc<dyn OidcProvider>))
                .collect();
            state = state.with_oauth(Arc::new(OAuthRuntime::new(&config, map)));
        }
        Providers::NativeApple => {
            let apple = fake("apple");
            let mut runtime = OAuthRuntime::new(&config, HashMap::new());
            runtime.native_apple = Some(apple.clone());
            state = state.with_oauth(Arc::new(runtime));
            native = Some(apple);
        }
    }
    (axon_api::router(state), native)
}

/// POST a form to an unauthenticated `/v1/oauth/*` route.
async fn post_form(
    app: &axum::Router,
    uri: &str,
    fields: &[(&str, &str)],
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields.iter().copied())
        .finish();
    let mut request =
        Request::post(uri).header("content-type", "application/x-www-form-urlencoded");
    if let Some(bearer) = bearer {
        request = request.header("authorization", format!("Bearer {bearer}"));
    }
    let mut request = request.body(Body::from(body)).unwrap();
    let peer: std::net::SocketAddr = "127.0.0.1:12345".parse().unwrap();
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).expect("JSON body"))
}

async fn call(
    app: &axum::Router,
    method: Method,
    uri: &str,
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(bearer) = bearer {
        request = request.header("authorization", format!("Bearer {bearer}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("JSON body")
    };
    (status, body)
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

async fn bind(store: &Store, provider: &str, email: Option<&str>) -> Uuid {
    store
        .bind_identity(provider, &Uuid::new_v4().to_string(), email)
        .await
        .unwrap()
        .id
}

/// An OAuth session's access token whose interactive sign-in was at `signed_in`.
async fn session(
    store: &Store,
    provider: &str,
    identity: Uuid,
    signed_in: Option<DateTime<Utc>>,
) -> String {
    store
        .issue_oauth_token(
            "session",
            Utc::now() + Duration::hours(1),
            provider,
            identity,
            CLIENT_ID,
            signed_in,
        )
        .await
        .unwrap()
        .token
}

fn long_ago() -> Option<DateTime<Utc>> {
    Some(Utc::now() - RECENT_SIGN_IN_WINDOW - Duration::minutes(1))
}

fn just_now() -> Option<DateTime<Utc>> {
    Some(Utc::now())
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn switched_off_every_management_route_is_403_and_status_says_so() {
    let store = store().await;
    clear_credentials(&store).await;
    let owner = store.issue_token("owner").await.unwrap();
    let identity = bind(&store, "google", None).await;
    let one = format!("{IDENTITIES}/{identity}");

    let off = app(store.clone(), false, Providers::Browser(&["google"]));
    for (method, uri) in [(Method::GET, IDENTITIES), (Method::DELETE, one.as_str())] {
        // The bearer gate still runs first: a stranger learns nothing.
        let (status, _) = call(&off, method.clone(), uri, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        let (status, body) = call(&off, method.clone(), uri, Some(&owner.token)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(code(&body), "management_disabled", "{method} {uri}");
    }
    assert!(
        store.find_identity_by_id(identity).await.unwrap().is_some(),
        "a disabled management API must not have unbound anything"
    );
    let (_, status) = call(&off, Method::GET, "/v1/status", Some(&owner.token)).await;
    assert_eq!(status["data"]["management"]["enabled"], false);

    let on = app(store.clone(), true, Providers::Browser(&["google"]));
    let (_, status) = call(&on, Method::GET, "/v1/status", Some(&owner.token)).await;
    assert_eq!(status["data"]["management"]["enabled"], true);
    let (status, _) = call(&on, Method::GET, IDENTITIES, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn the_list_marks_the_callers_identity_and_unusable_providers() {
    let store = store().await;
    clear_credentials(&store).await;
    let google = bind(&store, "google", Some("owner@example.com")).await;
    let apple = bind(&store, "apple", None).await;
    // A stale session may still read: only credential changes need step-up.
    let bearer = session(&store, "google", google, long_ago()).await;
    // Apple's provider is switched off on this server.
    let app = app(store.clone(), true, Providers::Browser(&["google"]));

    let (status, body) = call(&app, Method::GET, IDENTITIES, Some(&bearer)).await;
    assert_eq!(status, StatusCode::OK);
    let list = body["data"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    let entry = |id: Uuid| {
        list.iter()
            .find(|identity| identity["id"] == id.to_string())
            .unwrap()
    };
    assert_eq!(entry(google)["provider"], "google");
    assert_eq!(entry(google)["email"], "owner@example.com");
    assert_eq!(entry(google)["current"], true);
    assert_eq!(entry(google)["sign_in_available"], true);
    assert_eq!(entry(apple)["current"], false);
    assert_eq!(entry(apple)["sign_in_available"], false);
    assert_eq!(entry(apple)["email"], Value::Null);
    assert!(DateTime::parse_from_rfc3339(entry(apple)["linked_at"].as_str().unwrap()).is_ok());
    assert!(
        !body.to_string().contains("subject"),
        "the provider subject is not part of the contract"
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn unbinding_needs_a_recent_sign_in_or_a_non_expiring_token() {
    let store = store().await;
    clear_credentials(&store).await;
    let app = app(store.clone(), true, Providers::Browser(&["google"]));
    // Keeps the lockout guard out of this test's way.
    let cli = store.issue_token("cli").await.unwrap();
    let google = bind(&store, "google", None).await;

    // A stale session, and one that began before sign-in times were recorded.
    for signed_in in [long_ago(), None] {
        let target = bind(&store, "google", None).await;
        let stale = session(&store, "google", google, signed_in).await;
        let (status, body) = call(
            &app,
            Method::DELETE,
            &format!("{IDENTITIES}/{target}"),
            Some(&stale),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(code(&body), "recent_sign_in_required");
        assert!(store.find_identity_by_id(target).await.unwrap().is_some());
    }

    // A session that has just signed in.
    let target = bind(&store, "google", None).await;
    let fresh = session(&store, "google", google, just_now()).await;
    let uri = format!("{IDENTITIES}/{target}");
    let (status, body) = call(&app, Method::DELETE, &uri, Some(&fresh)).await;
    assert_eq!((status, &body), (StatusCode::NO_CONTENT, &Value::Null));
    assert!(store.find_identity_by_id(target).await.unwrap().is_none());
    // Gone is gone, and a malformed id is the caller's mistake.
    let (status, _) = call(&app, Method::DELETE, &uri, Some(&fresh)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("{IDENTITIES}/not-a-uuid"),
        Some(&fresh),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A non-expiring token needs no sign-in at all.
    let target = bind(&store, "google", None).await;
    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("{IDENTITIES}/{target}"),
        Some(&cli.token),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn unbinding_the_last_credential_is_refused_until_confirmed() {
    let store = store().await;
    clear_credentials(&store).await;
    let app = app(store.clone(), true, Providers::Browser(&["google"]));
    let google = bind(&store, "google", None).await;
    let bearer = session(&store, "google", google, just_now()).await;
    let uri = format!("{IDENTITIES}/{google}");

    let (status, body) = call(&app, Method::DELETE, &uri, Some(&bearer)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code(&body), "last_credential");
    // Refused means untouched: the session that asked still works.
    let (status, body) = call(&app, Method::GET, IDENTITIES, Some(&bearer)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"].as_array().unwrap().len(), 1);

    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("{uri}?allow_lockout=true"),
        Some(&bearer),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(store.list_identities().await.unwrap().is_empty());
    // Unbinding the identity you signed in with ends that session.
    let (status, _) = call(&app, Method::GET, IDENTITIES, Some(&bearer)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_second_identity_only_counts_if_its_provider_can_sign_in() {
    let store = store().await;
    clear_credentials(&store).await;
    let google = bind(&store, "google", None).await;
    let apple = bind(&store, "apple", None).await;
    let bearer = session(&store, "google", google, just_now()).await;
    let uri = format!("{IDENTITIES}/{google}");

    // Apple is bound, but this server has it switched off.
    let without_apple = app(store.clone(), true, Providers::Browser(&["google"]));
    let (status, body) = call(&without_apple, Method::DELETE, &uri, Some(&bearer)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code(&body), "last_credential");

    // With native Apple on, the Apple identity is a real way back in.
    let with_apple = app(store.clone(), true, Providers::NativeApple);
    let (status, _) = call(&with_apple, Method::DELETE, &uri, Some(&bearer)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(store.find_identity_by_id(apple).await.unwrap().is_some());

    // With OAuth off entirely, no identity is.
    let no_oauth = app(store.clone(), true, Providers::None);
    let cli = store.issue_token("cli").await.unwrap();
    store.revoke_token(cli.id).await.unwrap();
    let other = store.issue_token("other").await.unwrap();
    let (status, _) = call(
        &no_oauth,
        Method::DELETE,
        &format!("{IDENTITIES}/{apple}"),
        Some(&other.token),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "the caller's own non-expiring token survives"
    );
}

/// The native bind route must hold the same line, or it is the way around
/// step-up: bind an identity you control with a stolen bearer, then sign in
/// with it and arrive holding a fresh sign-in time.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn native_bind_needs_the_same_step_up() {
    let store = store().await;
    clear_credentials(&store).await;
    let app = app(store.clone(), true, Providers::NativeApple);
    let google = bind(&store, "google", None).await;

    let challenge = |bearer: String| {
        let app = app.clone();
        async move {
            let fields = [("client_id", CLIENT_ID), ("purpose", "bind")];
            post_form(&app, NATIVE_CHALLENGE, &fields, Some(&bearer)).await
        }
    };

    let (status, body) = challenge(session(&store, "google", google, long_ago()).await).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code(&body), "recent_sign_in_required");
    let pending: i64 =
        sqlx_core::query_scalar::query_scalar("SELECT count(*) FROM oauth_native_challenges")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(pending, 0, "a refused bind must not leave a challenge");

    let (status, _) = challenge(session(&store, "google", google, just_now()).await).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = challenge(store.issue_token("cli").await.unwrap().token).await;
    assert_eq!(status, StatusCode::OK);
}

/// The whole point of carrying the time along the chain: a refresh proves
/// possession of a refresh token, not that the owner is at the keyboard.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn refreshing_a_stale_session_does_not_make_it_recent() {
    let store = store().await;
    clear_credentials(&store).await;
    let app = app(store.clone(), true, Providers::Browser(&["google"]));
    store.issue_token("keeps-the-guard-quiet").await.unwrap();
    let google = bind(&store, "google", None).await;
    let target = bind(&store, "google", None).await;
    let refresh = format!("axon_rt_{}", Uuid::new_v4());
    store
        .issue_refresh_token(
            &axon_core::hash_secret(&refresh),
            google,
            CLIENT_ID,
            Utc::now() + Duration::days(30),
            long_ago(),
        )
        .await
        .unwrap();

    let mut refresh_token = refresh;
    for _ in 0..2 {
        let (status, body) = post_form(
            &app,
            "/v1/oauth/token",
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh_token),
                ("client_id", CLIENT_ID),
            ],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let access = body["access_token"].as_str().unwrap();
        refresh_token = body["refresh_token"].as_str().unwrap().to_owned();

        // A brand-new access token, and still not allowed to change credentials.
        let (status, body) = call(
            &app,
            Method::DELETE,
            &format!("{IDENTITIES}/{target}"),
            Some(access),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(code(&body), "recent_sign_in_required");
    }
    assert!(store.find_identity_by_id(target).await.unwrap().is_some());
}

/// The other half: a session minted by a real upstream sign-in is recent, and
/// stays recent across a refresh inside the window.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_fresh_upstream_sign_in_is_recent_and_survives_a_refresh() {
    let store = store().await;
    clear_credentials(&store).await;
    let (app, apple) = app_and_native_provider(store.clone(), true, Providers::NativeApple);
    let apple = apple.unwrap();
    store.issue_token("keeps-the-guard-quiet").await.unwrap();
    let subject = Uuid::new_v4().to_string();
    store.bind_identity("apple", &subject, None).await.unwrap();

    let (status, challenge) = post_form(
        &app,
        NATIVE_CHALLENGE,
        &[("client_id", CLIENT_ID), ("purpose", "login")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let identity_token =
        apple.sign_identity_token(&subject, None, challenge["nonce"].as_str(), None);
    let (status, pair) = post_form(
        &app,
        "/v1/oauth/apple/native/token",
        &[
            ("client_id", CLIENT_ID),
            ("challenge", challenge["challenge"].as_str().unwrap()),
            ("identity_token", &identity_token),
        ],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{pair}");

    let (status, refreshed) = post_form(
        &app,
        "/v1/oauth/token",
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", pair["refresh_token"].as_str().unwrap()),
            ("client_id", CLIENT_ID),
        ],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{refreshed}");

    for access in [&pair["access_token"], &refreshed["access_token"]] {
        let target = bind(&store, "apple", None).await;
        let (status, body) = call(
            &app,
            Method::DELETE,
            &format!("{IDENTITIES}/{target}"),
            access.as_str(),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
}
