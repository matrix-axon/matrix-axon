//! DB-gated tests for the management API (ADR 0109): the operator's switch,
//! the step-up rule for credential changes, listing and unbinding sign-in
//! identities, minting, listing and revoking tokens, and starting a bind.
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
const REDIRECT_URI: &str = "https://client.test/callback";
const IDENTITIES: &str = "/v1/management/oauth/identities";
const TOKENS: &str = "/v1/management/tokens";
const BINDS: &str = "/v1/management/oauth/binds";
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
    app_and_fakes(store, management, providers).0
}

/// The router, plus the fake provider behind each name (`"apple"` for
/// [`Providers::NativeApple`]) so a test can sign identity tokens for it.
fn app_and_fakes(
    store: Store,
    management: bool,
    providers: Providers,
) -> (axum::Router, HashMap<&'static str, Arc<TestOidcProvider>>) {
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
    let mut fakes = HashMap::new();
    let config = OauthConfig {
        enabled: true,
        external_base_url: Some("http://axon.test".to_owned()),
        access_token_ttl_secs: 3600,
        refresh_token_ttl_secs: 2_592_000,
        clients: vec![OauthClientConfig {
            client_id: CLIENT_ID.to_owned(),
            redirect_uris: vec![REDIRECT_URI.to_owned()],
        }],
        providers: axon_core::OauthProvidersConfig::default(),
    };
    match providers {
        Providers::None => {}
        Providers::Browser(names) => {
            fakes = names.iter().map(|name| (*name, fake(name))).collect();
            let map: HashMap<&'static str, Arc<dyn OidcProvider>> = fakes
                .iter()
                .map(|(name, fake)| (*name, fake.clone() as Arc<dyn OidcProvider>))
                .collect();
            state = state.with_oauth(Arc::new(OAuthRuntime::new(&config, map)));
        }
        Providers::NativeApple => {
            let apple = fake("apple");
            let mut runtime = OAuthRuntime::new(&config, HashMap::new());
            runtime.native_apple = Some(apple.clone());
            state = state.with_oauth(Arc::new(runtime));
            fakes.insert("apple", apple);
        }
    }
    (axon_api::router(state), fakes)
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

/// Send a JSON body, and return the response headers too.
async fn send_json(
    app: &axum::Router,
    uri: &str,
    bearer: &str,
    body: String,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let request = Request::post(uri)
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).expect("JSON body"),
    )
}

async fn mint(app: &axum::Router, bearer: &str, label: &str) -> (StatusCode, Value) {
    let body = serde_json::json!({ "label": label }).to_string();
    let (status, _, body) = send_json(app, TOKENS, bearer, body).await;
    (status, body)
}

async fn start_bind(app: &axum::Router, bearer: &str, provider: &str) -> (StatusCode, Value) {
    let body = serde_json::json!({ "provider": provider }).to_string();
    let (status, _, body) = send_json(app, BINDS, bearer, body).await;
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

    let token = format!("{TOKENS}/{}", owner.id);
    let a_bind = format!("{BINDS}/{}", Uuid::new_v4());

    let off = app(store.clone(), false, Providers::Browser(&["google"]));
    for (method, uri) in [
        (Method::GET, IDENTITIES),
        (Method::DELETE, one.as_str()),
        (Method::GET, TOKENS),
        (Method::POST, TOKENS),
        (Method::DELETE, token.as_str()),
        (Method::POST, BINDS),
        (Method::GET, a_bind.as_str()),
    ] {
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
    assert!(
        store.verify_token(&owner.token).await.unwrap().is_some(),
        "a disabled management API must not have revoked anything"
    );
    assert_eq!(store.list_tokens().await.unwrap().len(), 1);
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
    // Most recently linked first, as documented: Apple was bound after Google.
    assert_eq!(
        [&list[0]["id"], &list[1]["id"]],
        [&apple.to_string(), &google.to_string()]
    );
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
    let (app, fakes) = app_and_fakes(store.clone(), true, Providers::NativeApple);
    let apple = fakes["apple"].clone();
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

const HALF_AN_HOUR: i64 = 30 * 60;

/// Unbind a throwaway identity with `access`, reporting how the server ruled.
async fn try_credential_change(
    app: &axum::Router,
    store: &Store,
    access: &str,
) -> (StatusCode, String) {
    let target = bind(store, "google", None).await;
    let (status, body) = call(
        app,
        Method::DELETE,
        &format!("{IDENTITIES}/{target}"),
        Some(access),
    )
    .await;
    (status, code(&body).to_owned())
}

fn refused() -> (StatusCode, String) {
    (StatusCode::FORBIDDEN, "recent_sign_in_required".to_owned())
}

fn allowed() -> (StatusCode, String) {
    (StatusCode::NO_CONTENT, String::new())
}

/// An identity token stays valid long after it is issued. Redeeming one must
/// never read as a fresh sign-in: the session's sign-in time is whatever the
/// provider's signature vouches for, and when that is nothing, it is unknown.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn redeeming_an_old_identity_token_is_not_a_recent_sign_in() {
    let store = store().await;
    clear_credentials(&store).await;
    let (app, fakes) = app_and_fakes(store.clone(), true, Providers::Browser(&["google"]));
    let google = fakes["google"].clone();
    store.issue_token("keeps-the-guard-quiet").await.unwrap();
    let subject = Uuid::new_v4().to_string();
    store.bind_identity("google", &subject, None).await.unwrap();

    // The nonce-free identity-token grant, as a native SDK would use it.
    let redeem = |identity_token: String| {
        let app = app.clone();
        async move {
            let (status, body) = post_form(
                &app,
                "/v1/oauth/token",
                &[
                    ("grant_type", "urn:axon:identity_token"),
                    ("provider", "google"),
                    ("identity_token", &identity_token),
                    ("client_id", CLIENT_ID),
                ],
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body["access_token"].as_str().unwrap().to_owned()
        }
    };

    // Issued half an hour ago, redeemed now: the case a stolen, unredeemed
    // token presents. It signs in, and that is all it does.
    let old =
        redeem(google.sign_identity_token_issued_ago(&subject, None, HALF_AN_HOUR, None)).await;
    assert_eq!(try_credential_change(&app, &store, &old).await, refused());
    let old = redeem(google.sign_identity_token_issued_ago(
        &subject,
        None,
        HALF_AN_HOUR,
        Some(HALF_AN_HOUR),
    ))
    .await;
    assert_eq!(try_credential_change(&app, &store, &old).await, refused());

    // Issued just now, but nonce-free and silent about `auth_time`: nothing
    // ties it to a sign-in this server started. Unknown is not recent.
    let unknown = redeem(google.sign_identity_token_issued_ago(&subject, None, 0, None)).await;
    assert_eq!(
        try_credential_change(&app, &store, &unknown).await,
        refused()
    );

    // Freshly issued for an authentication that happened long ago (a
    // provider session being reused): `auth_time` wins over `iat`.
    let reused =
        redeem(google.sign_identity_token_issued_ago(&subject, None, 0, Some(HALF_AN_HOUR))).await;
    assert_eq!(
        try_credential_change(&app, &store, &reused).await,
        refused()
    );

    // The provider itself says the owner authenticated a minute ago.
    let fresh = redeem(google.sign_identity_token_issued_ago(&subject, None, 30, Some(60))).await;
    assert_eq!(try_credential_change(&app, &store, &fresh).await, allowed());
}

/// The native flow binds the token to a server nonce, which proves the token
/// was issued for this sign-in. It does not excuse an `auth_time` that says
/// the owner authenticated long before.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_nonce_bound_token_still_answers_to_its_auth_time() {
    let store = store().await;
    clear_credentials(&store).await;
    let (app, fakes) = app_and_fakes(store.clone(), true, Providers::NativeApple);
    let apple = fakes["apple"].clone();
    store.issue_token("keeps-the-guard-quiet").await.unwrap();
    let subject = Uuid::new_v4().to_string();
    store.bind_identity("apple", &subject, None).await.unwrap();

    for (auth_time_ago, expected) in [(Some(HALF_AN_HOUR), refused()), (Some(60), allowed())] {
        let (_, challenge) = post_form(
            &app,
            NATIVE_CHALLENGE,
            &[("client_id", CLIENT_ID), ("purpose", "login")],
            None,
        )
        .await;
        let identity_token = apple.sign_identity_token_issued_ago(
            &subject,
            challenge["nonce"].as_str(),
            0,
            auth_time_ago,
        );
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
        let access = pair["access_token"].as_str().unwrap();
        assert_eq!(try_credential_change(&app, &store, access).await, expected);
    }
}

/// The browser flow verifies the identity token at the callback and mints
/// later, at code redemption. The session must carry the time verified at the
/// callback, not the moment the code was redeemed.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn the_browser_flow_carries_the_verified_time_to_code_redemption() {
    let store = store().await;
    clear_credentials(&store).await;
    let (app, fakes) = app_and_fakes(store.clone(), true, Providers::Browser(&["google"]));
    let google = fakes["google"].clone();
    store.issue_token("keeps-the-guard-quiet").await.unwrap();
    let subject = Uuid::new_v4().to_string();
    store.bind_identity("google", &subject, None).await.unwrap();

    let get = |uri: String| {
        let app = app.clone();
        async move {
            let mut request = Request::get(uri).body(Body::empty()).unwrap();
            let peer: std::net::SocketAddr = "127.0.0.1:12345".parse().unwrap();
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(peer));
            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            url::Url::parse(response.headers()["location"].to_str().unwrap()).unwrap()
        }
    };
    let param = |url: &url::Url, name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
            .unwrap()
    };
    // PKCE S256 over a fixed verifier.
    let verifier = "management-test-pkce-verifier-0123456789-abcdef";
    let pkce = {
        use base64::Engine;
        use sha2::Digest;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(verifier.as_bytes()))
    };

    // `backdate` rewinds what the callback recorded, standing in for a code
    // that sat unredeemed: redemption must use the record, not its own clock.
    for (backdate, expected) in [(false, allowed()), (true, refused())] {
        let redirect: String =
            url::form_urlencoded::byte_serialize(REDIRECT_URI.as_bytes()).collect();
        let upstream = get(format!(
            "/v1/oauth/authorize?response_type=code&client_id={CLIENT_ID}&redirect_uri={redirect}\
             &code_challenge={pkce}&code_challenge_method=S256&provider=google&state=s"
        ))
        .await;
        let code = google.issue_code(&subject, None, &param(&upstream, "nonce"));
        let back = get(format!(
            "/v1/oauth/google/callback?code={code}&state={}",
            param(&upstream, "state")
        ))
        .await;
        let recorded: Option<DateTime<Utc>> = sqlx_core::query_scalar::query_scalar(
            "SELECT authenticated_at FROM oauth_authorization_requests WHERE status = 'code_issued'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert!(
            recorded.is_some_and(|at| (Utc::now() - at).num_seconds().abs() < 120),
            "the callback records the verified time: {recorded:?}"
        );
        if backdate {
            sqlx_core::query::query(
                "UPDATE oauth_authorization_requests \
                    SET authenticated_at = now() - interval '30 minutes' \
                  WHERE status = 'code_issued'",
            )
            .execute(store.pool())
            .await
            .unwrap();
        }
        let (status, pair) = post_form(
            &app,
            "/v1/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("code", &param(&back, "code")),
                ("code_verifier", verifier),
                ("client_id", CLIENT_ID),
                ("redirect_uri", REDIRECT_URI),
            ],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{pair}");
        let access = pair["access_token"].as_str().unwrap();
        assert_eq!(try_credential_change(&app, &store, access).await, expected);
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn minting_returns_the_secret_once_and_records_who_minted() {
    let store = store().await;
    clear_credentials(&store).await;
    let app = app(store.clone(), true, Providers::None);
    let owner = store.issue_token("owner").await.unwrap();

    let body = serde_json::json!({ "label": "  laptop  " }).to_string();
    let (status, headers, body) = send_json(&app, TOKENS, &owner.token, body).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(headers["referrer-policy"], "no-referrer");
    assert_eq!(body["data"]["label"], "laptop", "the label is trimmed");
    let secret = body["data"]["token"].as_str().unwrap().to_owned();
    let id = body["data"]["id"].as_str().unwrap().to_owned();

    // It is a working bearer, and the list shows it without the secret.
    let (status, list) = call(&app, Method::GET, TOKENS, Some(&secret)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !list.to_string().contains(&secret) && !list.to_string().contains("hash"),
        "the list never carries a secret"
    );
    let list = list["data"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    let entry = |id: &str| list.iter().find(|token| token["id"] == id).unwrap();
    let minted = entry(&id);
    assert_eq!(minted["current"], true);
    assert_eq!(minted["created_by_token_id"], owner.id.to_string());
    assert_eq!(minted["expires_at"], Value::Null);
    assert_eq!(minted["revoked_at"], Value::Null);
    assert!(DateTime::parse_from_rfc3339(minted["created_at"].as_str().unwrap()).is_ok());
    assert!(DateTime::parse_from_rfc3339(minted["last_used_at"].as_str().unwrap()).is_ok());
    let owner_entry = entry(&owner.id.to_string());
    assert_eq!(owner_entry["current"], false);
    assert_eq!(owner_entry["created_by_token_id"], Value::Null);

    // Non-expiring, so it may change credentials itself.
    let (status, _) = mint(&app, &secret, "phone").await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_mint_is_refused_for_a_stale_session_and_for_a_bad_label() {
    let store = store().await;
    clear_credentials(&store).await;
    let app = app(store.clone(), true, Providers::Browser(&["google"]));
    let google = bind(&store, "google", None).await;
    let stale = session(&store, "google", google, long_ago()).await;
    let fresh = session(&store, "google", google, just_now()).await;

    let (status, body) = mint(&app, &stale, "laptop").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code(&body), "recent_sign_in_required");
    // The step-up check runs before the body is read: no hint about it leaks.
    let (status, _, body) = send_json(&app, TOKENS, &stale, "not json".to_owned()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code(&body), "recent_sign_in_required");

    for label in ["", "   ", &"x".repeat(81), "esc\u{1b}[2J", "two\nlines"] {
        let (status, body) = mint(&app, &fresh, label).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label:?}");
        assert_eq!(code(&body), "bad_request", "{label:?}");
    }
    for body in [
        r#"{"label":"ok","expires_at":"2099-01-01T00:00:00Z"}"#.to_owned(),
        "{}".to_owned(),
    ] {
        let (status, _, _) = send_json(&app, TOKENS, &fresh, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let huge = serde_json::json!({ "label": "x".repeat(8 * 1024) }).to_string();
    let (status, _, body) = send_json(&app, TOKENS, &fresh, huge).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(code(&body), "payload_too_large");
    assert_eq!(
        store.list_tokens().await.unwrap().len(),
        2,
        "nothing was minted by any refused request"
    );

    let (status, body) = mint(&app, &fresh, &"é".repeat(80)).await;
    assert_eq!(status, StatusCode::CREATED);
    let minted = Uuid::parse_str(body["data"]["id"].as_str().unwrap()).unwrap();
    let fresh_id = store.verify_token(&fresh).await.unwrap().unwrap().id;
    let listed = store.list_tokens().await.unwrap();
    let row = listed.iter().find(|token| token.id == minted).unwrap();
    assert_eq!(row.created_by_token_id, Some(fresh_id));
    assert_eq!(row.expires_at, None);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn revoking_needs_step_up_and_stops_the_token_at_once() {
    let store = store().await;
    clear_credentials(&store).await;
    let app = app(store.clone(), true, Providers::Browser(&["google"]));
    let google = bind(&store, "google", None).await;
    let stale = session(&store, "google", google, long_ago()).await;
    let owner = store.issue_token("owner").await.unwrap();
    let phone = store.issue_token("phone").await.unwrap();
    let uri = format!("{TOKENS}/{}", phone.id);

    let (status, body) = call(&app, Method::DELETE, &uri, Some(&stale)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code(&body), "recent_sign_in_required");
    let (status, _) = call(&app, Method::GET, TOKENS, Some(&phone.token)).await;
    assert_eq!(status, StatusCode::OK, "a refused revoke changes nothing");

    let (status, _) = call(&app, Method::DELETE, &uri, Some(&owner.token)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&app, Method::GET, TOKENS, Some(&phone.token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Still listed, as revoked; and revoking it again is not an error.
    let (_, list) = call(&app, Method::GET, TOKENS, Some(&owner.token)).await;
    let entry = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|token| token["id"] == phone.id.to_string())
        .unwrap()
        .clone();
    assert!(entry["revoked_at"].is_string());
    for uri in [uri.clone(), format!("{uri}?allow_lockout=true")] {
        let (status, _) = call(&app, Method::DELETE, &uri, Some(&owner.token)).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{uri}");
    }

    for uri in [
        format!("{TOKENS}/{}", Uuid::new_v4()),
        format!("{TOKENS}/{}?allow_lockout=true", Uuid::new_v4()),
    ] {
        let (status, body) = call(&app, Method::DELETE, &uri, Some(&owner.token)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(code(&body), "not_found", "{uri}");
    }
    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("{TOKENS}/not-a-uuid"),
        Some(&owner.token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn revoking_the_last_credential_is_refused_until_confirmed() {
    let store = store().await;
    clear_credentials(&store).await;
    // No OAuth: tokens are the only credentials there can be.
    let app = app(store.clone(), true, Providers::None);
    let owner = store.issue_token("owner").await.unwrap();
    let spare = store.issue_token("spare").await.unwrap();
    let own = format!("{TOKENS}/{}", owner.id);

    // With a spare, a device may sign itself out.
    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("{TOKENS}/{}", spare.id),
        Some(&spare.token),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&app, Method::GET, TOKENS, Some(&spare.token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Without one, it is the last way in.
    let (status, body) = call(&app, Method::DELETE, &own, Some(&owner.token)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code(&body), "last_credential");
    let (status, _) = call(&app, Method::GET, TOKENS, Some(&owner.token)).await;
    assert_eq!(status, StatusCode::OK, "refused means untouched");

    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("{own}?allow_lockout=true"),
        Some(&owner.token),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&app, Method::GET, TOKENS, Some(&owner.token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_token_may_go_when_a_usable_identity_remains_and_sessions_always_may() {
    let store = store().await;
    clear_credentials(&store).await;
    let google = bind(&store, "google", None).await;
    let owner = store.issue_token("owner").await.unwrap();
    let own = format!("{TOKENS}/{}", owner.id);

    // Google is switched off here, so the identity is no way back in.
    let off = app(store.clone(), true, Providers::Browser(&["apple"]));
    let (status, body) = call(&off, Method::DELETE, &own, Some(&owner.token)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code(&body), "last_credential");

    // A session's own access token was never a survivor: revoking it is not
    // what would lock the owner out, so it is not refused.
    let fresh = session(&store, "google", google, just_now()).await;
    let fresh_id = store.verify_token(&fresh).await.unwrap().unwrap().id;
    let (status, _) = call(
        &off,
        Method::DELETE,
        &format!("{TOKENS}/{fresh_id}"),
        Some(&fresh),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let on = app(store.clone(), true, Providers::Browser(&["google"]));
    let (status, _) = call(&on, Method::DELETE, &own, Some(&owner.token)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_bind_starts_only_for_a_provider_this_server_can_redirect_to() {
    let store = store().await;
    clear_credentials(&store).await;
    let owner = store.issue_token("owner").await.unwrap();
    let pending = || async {
        sqlx_core::query::query("SELECT 1 FROM oauth_bind_requests")
            .fetch_all(store.pool())
            .await
            .unwrap()
            .len()
    };

    let off = app(store.clone(), true, Providers::None);
    let (status, body) = start_bind(&off, &owner.token, "google").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code(&body), "bind_unavailable");

    let app = app(store.clone(), true, Providers::Browser(&["google"]));
    let (status, body) = start_bind(&app, &owner.token, "microsoft").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code(&body), "bind_unavailable");
    for unknown in ["github", "Google", ""] {
        let (status, body) = start_bind(&app, &owner.token, unknown).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{unknown:?}");
        assert_eq!(code(&body), "bad_request", "{unknown:?}");
    }

    // Native-only Apple has no browser provider for the bind to redirect to.
    let native = self::app(store.clone(), true, Providers::NativeApple);
    let (status, body) = start_bind(&native, &owner.token, "apple").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code(&body), "bind_unavailable");

    // A stale session may not start one, whatever the provider.
    let google = bind(&store, "google", None).await;
    let stale = session(&store, "google", google, long_ago()).await;
    let (status, body) = start_bind(&app, &stale, "google").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code(&body), "recent_sign_in_required");

    assert_eq!(pending().await, 0, "no refused request created a bind");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_started_bind_drives_the_existing_browser_leg_and_reports_its_status() {
    let store = store().await;
    clear_credentials(&store).await;
    let app = app(store.clone(), true, Providers::Browser(&["google"]));
    let google = bind(&store, "google", None).await;
    let fresh = session(&store, "google", google, just_now()).await;
    let stale = session(&store, "google", google, long_ago()).await;

    let body = serde_json::json!({ "provider": "google" }).to_string();
    let (status, headers, body) = send_json(&app, BINDS, &fresh, body).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["cache-control"], "no-store");
    let started = &body["data"];
    assert_eq!(started["provider"], "google");
    assert_eq!(started["status"], "pending");
    assert_eq!(started["identity_id"], Value::Null);
    let id = Uuid::parse_str(started["id"].as_str().unwrap()).unwrap();
    let expires = DateTime::parse_from_rfc3339(started["expires_at"].as_str().unwrap()).unwrap();
    let left = expires.with_timezone(&Utc) - Utc::now();
    assert!(
        left > Duration::minutes(9) && left <= Duration::minutes(10),
        "the shared ten-minute handshake lifetime, got {left}"
    );

    // The URL is the existing unauthenticated browser leg, and it redirects
    // to the provider with this bind as its state.
    let url = url::Url::parse(started["url"].as_str().unwrap()).unwrap();
    assert_eq!(url.origin().ascii_serialization(), "http://axon.test");
    assert_eq!(url.path(), "/v1/oauth/bind");
    let mut request = Request::get(format!("{}?{}", url.path(), url.query().unwrap()))
        .body(Body::empty())
        .unwrap();
    let peer: std::net::SocketAddr = "127.0.0.1:12345".parse().unwrap();
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));
    let response = app.clone().oneshot(request).await.unwrap();
    assert!(response.status().is_redirection(), "{}", response.status());
    let location = response.headers()["location"].to_str().unwrap();
    assert!(location.starts_with("https://fake-idp.test/authorize"));
    assert!(location.contains(&id.to_string()));

    // Reading status is not a credential change: a stale session may poll.
    let uri = format!("{BINDS}/{id}");
    let (status, body) = call(&app, Method::GET, &uri, Some(&stale)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["status"], "pending");
    assert!(
        body["data"].get("url").is_none(),
        "status does not repeat the URL"
    );

    // The callback's own completion step, as the browser leg would run it.
    let bound = store
        .complete_bind_request(id, "google", "new-subject", Some("new@example.com"))
        .await
        .unwrap()
        .expect("still pending");
    let (_, body) = call(&app, Method::GET, &uri, Some(&stale)).await;
    assert_eq!(body["data"]["status"], "completed");
    assert_eq!(body["data"]["identity_id"], bound.to_string());

    let (status, body) = call(
        &app,
        Method::GET,
        &format!("{BINDS}/{}", Uuid::new_v4()),
        Some(&stale),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code(&body), "not_found");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_lapsed_or_canceled_bind_reads_as_expired() {
    let store = store().await;
    clear_credentials(&store).await;
    let app = app(store.clone(), true, Providers::Browser(&["google"]));
    let owner = store.issue_token("owner").await.unwrap();
    let id_of = |body: &Value| Uuid::parse_str(body["data"]["id"].as_str().unwrap()).unwrap();

    // Canceled, as a failed sign-in at the callback leaves it.
    let (_, body) = start_bind(&app, &owner.token, "google").await;
    let canceled = id_of(&body);
    assert!(store.cancel_bind_request(canceled).await.unwrap());
    // Still `pending` in the table, but past its time: nothing rewrites it.
    let (_, body) = start_bind(&app, &owner.token, "google").await;
    let lapsed = id_of(&body);
    sqlx_core::query::query(
        "UPDATE oauth_bind_requests SET expires_at = now() - interval '1 second' \
          WHERE device_code = $1",
    )
    .bind(lapsed)
    .execute(store.pool())
    .await
    .unwrap();

    for id in [canceled, lapsed] {
        let (status, body) = call(
            &app,
            Method::GET,
            &format!("{BINDS}/{id}"),
            Some(&owner.token),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["status"], "expired");
    }

    // Starting another sweeps the lapsed one away; reading it is then a 404.
    start_bind(&app, &owner.token, "google").await;
    let (status, _) = call(
        &app,
        Method::GET,
        &format!("{BINDS}/{lapsed}"),
        Some(&owner.token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
