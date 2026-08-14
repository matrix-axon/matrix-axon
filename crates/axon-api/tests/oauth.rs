//! DB-gated OAuth integration test (M14b, ADR 0054): drives the full Path A
//! authorize -> callback -> token -> refresh chain, Path B's identity-token
//! grant (with replay rejection), the catch-all-precedence regression, and a
//! check that pre-existing CLI-minted tokens still verify unchanged.
//!
//! Needs a database and is `#[ignore]`d by default, same as `tests/http.rs`:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon \
//!   cargo test -p axon-api --test oauth -- --ignored --test-threads=1
//! ```

mod common;

use std::sync::Arc;

use axon_api::{AppState, BootstrapConfig, OAuthRuntime, OidcProvider};
use axon_core::{OauthClientConfig, OauthConfig};
use axon_store::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use chrono::{Duration, Utc};
use common::oidc::TestOidcProvider;
use common::{
    StubDeviceList, StubLifecycle, StubMediaProxy, StubSender, StubTrust, StubVerification,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use uuid::Uuid;

async fn store() -> Store {
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
    Store::connect(&url, 5).await.expect("connect + migrate")
}

const CLIENT_ID: &str = "test-client";
const REDIRECT_URI: &str = "axon://oauth/callback";
const TEST_PROVIDER: &str = "test";
const TEST_ISSUER: &str = "https://fake-idp.test/";
const TEST_AUDIENCE: &str = "test-upstream-client-id";

/// A real [`TokenVerifier`](axon_api::TokenVerifier) backed by `store` — unlike
/// most HTTP tests (which stub it out), this test needs the genuine DB-backed
/// verifier so a token minted through `/v1/oauth/token` actually round-trips
/// through `verify_token`'s new `expires_at` clause.
fn real_verifier(store: Store) -> Arc<dyn axon_api::TokenVerifier> {
    Arc::new(axon_api::StoreTokenVerifier::new(store))
}

/// Build the router with oauth enabled, one `TestOidcProvider` registered as
/// `"test"`, and one statically pre-registered client. Returns the router
/// plus the same `TestOidcProvider` instance (so the test can drive its
/// `issue_code`/`sign_identity_token` stand-ins for "the browser/native SDK
/// did its part").
fn app_with_oauth(store: Store) -> (axum::Router, Arc<TestOidcProvider>) {
    app_with_oauth_bootstrap(store, None)
}

fn app_with_oauth_bootstrap(
    store: Store,
    bootstrap: Option<BootstrapConfig>,
) -> (axum::Router, Arc<TestOidcProvider>) {
    let provider = Arc::new(TestOidcProvider::new(
        TEST_PROVIDER,
        TEST_ISSUER,
        TEST_AUDIENCE,
    ));
    let mut providers: std::collections::HashMap<&'static str, Arc<dyn OidcProvider>> =
        std::collections::HashMap::new();
    providers.insert(TEST_PROVIDER, provider.clone() as Arc<dyn OidcProvider>);

    let oauth_config = OauthConfig {
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
    let runtime = Arc::new(OAuthRuntime::new(&oauth_config, providers));

    let (live, _rx) = tokio::sync::broadcast::channel(16);
    let mut state = AppState::new(
        store.clone(),
        live,
        Arc::new(StubSender::ok("$unused:localhost")),
        Arc::new(StubLifecycle::ok(Uuid::nil())),
        Arc::new(StubVerification::ok("$unused-flow")),
        Arc::new(StubTrust::ok()),
        Arc::new(StubDeviceList::ok()),
        real_verifier(store),
        Arc::new(StubMediaProxy),
        None,
    )
    .with_oauth(runtime);
    if let Some(bootstrap) = bootstrap {
        state = state.with_bootstrap(bootstrap);
    }
    let app = axon_api::router(state);
    (app, provider)
}

/// PKCE S256: a random verifier and its matching challenge.
fn pkce_pair() -> (String, String) {
    let verifier = "test-pkce-verifier-0123456789-abcdefghijklmno";
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    (verifier.to_owned(), challenge)
}

fn query_param(url: &url::Url, name: &str) -> String {
    url.query_pairs()
        .find(|(k, _)| k == name)
        .unwrap_or_else(|| panic!("missing query param {name:?} in {url}"))
        .1
        .into_owned()
}

/// `/v1/oauth/*`'s rate-limit layer extracts `ConnectInfo<SocketAddr>`, which
/// a real deployment gets from `axum::serve`'s
/// `into_make_service_with_connect_info::<SocketAddr>()`. Driving the router
/// directly via `tower::oneshot` (as these tests do) bypasses that, so it must
/// be inserted into the request's extensions by hand — otherwise axum 500s
/// before the handler ever runs.
fn with_fake_connect_info(mut req: Request<Body>) -> Request<Body> {
    let addr: std::net::SocketAddr = "127.0.0.1:12345".parse().unwrap();
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo(addr));
    req
}

async fn get_no_body(app: &axum::Router, uri: &str) -> axum::response::Response {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    app.clone()
        .oneshot(with_fake_connect_info(req))
        .await
        .expect("request")
}

async fn post_form(app: &axum::Router, uri: &str, form: &[(&str, &str)]) -> (StatusCode, Value) {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let resp = app
        .clone()
        .oneshot(with_fake_connect_info(req))
        .await
        .expect("request");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    let json = serde_json::from_slice(&bytes).expect("json body");
    (status, json)
}

async fn post_form_text(
    app: &axum::Router,
    uri: &str,
    form: &[(&str, &str)],
) -> (StatusCode, String) {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let resp = app
        .clone()
        .oneshot(with_fake_connect_info(req))
        .await
        .expect("request");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
    )
}

async fn get_text(app: &axum::Router, uri: &str) -> (StatusCode, String) {
    let resp = get_no_body(app, uri).await;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
    )
}

/// Like [`with_fake_connect_info`] but lets the caller pick the peer address,
/// so a test can drive the `require_allowed_peer` gate from a non-loopback
/// source.
fn with_connect_info_addr(mut req: Request<Body>, addr: &str) -> Request<Body> {
    let addr: std::net::SocketAddr = addr.parse().unwrap();
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo(addr));
    req
}

async fn get_text_from(app: &axum::Router, uri: &str, peer: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app
        .clone()
        .oneshot(with_connect_info_addr(req, peer))
        .await
        .expect("request");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
    )
}

async fn post_form_text_from(
    app: &axum::Router,
    uri: &str,
    form: &[(&str, &str)],
    peer: &str,
) -> (StatusCode, String) {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let resp = app
        .clone()
        .oneshot(with_connect_info_addr(req, peer))
        .await
        .expect("request");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
    )
}

async fn reset_bootstrap_tables(store: &Store) {
    for sql in [
        "DELETE FROM oauth_authorization_requests",
        "DELETE FROM oauth_bind_requests",
        "DELETE FROM oauth_refresh_tokens",
        "DELETE FROM tokens",
        "DELETE FROM oauth_identities",
        "DELETE FROM accounts",
    ] {
        sqlx_core::query::query(sql)
            .execute(store.pool())
            .await
            .unwrap();
    }
}

fn bootstrap_config() -> BootstrapConfig {
    BootstrapConfig::new(
        false,
        "ABC234".to_owned(),
        Some("https://web.test/app".to_owned()),
    )
}

fn bootstrap_config_allow_remote() -> BootstrapConfig {
    BootstrapConfig::new(true, "ABC234".to_owned(), None)
}

/// An arbitrary non-loopback address for exercising `bootstrap_web_allow_remote`.
const REMOTE_PEER: &str = "203.0.113.7:54321";

fn extract_bootstrap_token(body: &str) -> String {
    body.split_once(r#"id="axon-bootstrap-token""#)
        .and_then(|(_, rest)| rest.split_once('>'))
        .and_then(|(_, rest)| rest.split_once("</pre>"))
        .map(|(token, _)| token)
        .expect("bootstrap token element")
        .to_owned()
}

#[test]
fn extract_bootstrap_token_uses_element_id() {
    let body =
        r#"<pre>wrong</pre><pre id="axon-bootstrap-token" data-kind="bearer">axon_right</pre>"#;
    assert_eq!(extract_bootstrap_token(body), "axon_right");
}

async fn get_authed(app: &axum::Router, uri: &str, token: &str) -> StatusCode {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.expect("request").status()
}

#[tokio::test]
#[ignore = "requires empty Postgres"]
async fn bootstrap_bearer_token_mints_once_and_then_closes() {
    let store = store().await;
    reset_bootstrap_tables(&store).await;
    let (app, _provider) = app_with_oauth_bootstrap(store.clone(), Some(bootstrap_config()));

    let (status, body) = get_text(&app, "/bootstrap/ABC234").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Create bearer token"));
    assert!(body.contains("Continue with test"));

    let (status, body) = post_form_text(
        &app,
        "/bootstrap/ABC234/token",
        &[("label", "browser setup")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = extract_bootstrap_token(&body);
    assert!(token.starts_with("axon_"));
    assert!(body.contains(r#"href="https://web.test/app""#));
    assert!(!body.contains(&format!("https://web.test/app?token={token}")));
    assert!(
        store.verify_token(&token).await.expect("verify").is_some(),
        "the browser-shown token should verify"
    );

    let (status, _body) =
        post_form_text(&app, "/bootstrap/ABC234/token", &[("label", "second")]).await;
    assert_eq!(status, StatusCode::CONFLICT);

    reset_bootstrap_tables(&store).await;
}

#[tokio::test]
#[ignore = "requires empty Postgres"]
async fn bootstrap_sso_binds_identity_and_returns_token_pair() {
    let store = store().await;
    reset_bootstrap_tables(&store).await;
    let (app, provider) = app_with_oauth_bootstrap(store.clone(), Some(bootstrap_config()));

    let resp = get_no_body(&app, "/bootstrap/ABC234/oauth/test").await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp
        .headers()
        .get("location")
        .expect("Location header")
        .to_str()
        .unwrap()
        .to_owned();
    let upstream_url = url::Url::parse(&location).expect("upstream redirect is a URL");
    let upstream_state = query_param(&upstream_url, "state");
    let upstream_nonce = query_param(&upstream_url, "nonce");
    assert!(upstream_state.starts_with("bootstrap:"));

    let subject = format!("bootstrap-subject-{}", Uuid::new_v4());
    let code = provider.issue_code(&subject, Some("owner@example.com"), &upstream_nonce);
    let (status, body) = get_text(
        &app,
        &format!("/v1/oauth/test/callback?code={code}&state={upstream_state}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("SSO configured"));
    assert!(body.contains("Access token"));
    assert!(body.contains("Refresh token"));
    assert!(body.contains(r#"href="https://web.test/app""#));

    let identity = store
        .find_identity(TEST_PROVIDER, &subject)
        .await
        .expect("find identity")
        .expect("identity should be bound");
    assert_eq!(identity.email.as_deref(), Some("owner@example.com"));
    let listed = store.list_tokens().await.expect("list tokens");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].provider.as_deref(), Some(TEST_PROVIDER));

    let resp = get_no_body(&app, "/bootstrap/ABC234/oauth/test").await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    reset_bootstrap_tables(&store).await;
}

#[tokio::test]
#[ignore = "requires empty Postgres"]
async fn bootstrap_wrong_urls_lock_the_web_surface() {
    let store = store().await;
    reset_bootstrap_tables(&store).await;
    let (app, _provider) = app_with_oauth_bootstrap(store.clone(), Some(bootstrap_config()));

    let (status, _body) = post_form_text(&app, "/bootstrap/token", &[("label", "old path")]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    for _ in 0..4 {
        let (status, _body) = get_text(&app, "/bootstrap/WRONG2").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    let (status, _body) = get_text(&app, "/bootstrap/WRONG2").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    let (status, _body) = get_text(&app, "/bootstrap/ABC234").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    let (status, _body) =
        post_form_text(&app, "/bootstrap/ABC234/token", &[("label", "locked")]).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    reset_bootstrap_tables(&store).await;
}

#[tokio::test]
#[ignore = "requires empty Postgres"]
async fn bootstrap_allow_remote_permits_non_loopback_peer() {
    let store = store().await;
    reset_bootstrap_tables(&store).await;

    // Default (`allow_remote = false`): a non-loopback peer is turned away
    // before it ever reaches the handler.
    let (default_app, _provider) =
        app_with_oauth_bootstrap(store.clone(), Some(bootstrap_config()));
    let (status, _body) = get_text_from(&default_app, "/bootstrap/ABC234", REMOTE_PEER).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // `allow_remote = true`: the same peer can walk the full bootstrap flow
    // and mint the first credential.
    let (app, _provider) =
        app_with_oauth_bootstrap(store.clone(), Some(bootstrap_config_allow_remote()));

    let (status, body) = get_text_from(&app, "/bootstrap/ABC234", REMOTE_PEER).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Create bearer token"));

    let (status, body) = post_form_text_from(
        &app,
        "/bootstrap/ABC234/token",
        &[("label", "remote setup")],
        REMOTE_PEER,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = extract_bootstrap_token(&body);
    assert!(
        store.verify_token(&token).await.expect("verify").is_some(),
        "the remotely-minted token should verify"
    );

    reset_bootstrap_tables(&store).await;
}

#[tokio::test]
#[ignore]
async fn path_a_authorize_callback_token_refresh_chain() {
    let store = store().await;
    let (app, provider) = app_with_oauth(store.clone());

    let subject = format!("test-subject-{}", Uuid::new_v4());
    store
        .bind_identity(TEST_PROVIDER, &subject, Some("owner@example.com"))
        .await
        .expect("bind identity");

    let (verifier, challenge) = pkce_pair();
    let authorize_uri = format!(
        "/v1/oauth/authorize?response_type=code&client_id={CLIENT_ID}&redirect_uri={}&code_challenge={challenge}&code_challenge_method=S256&provider={TEST_PROVIDER}&state=client-state-abc",
        urlencoding_encode(REDIRECT_URI),
    );
    let resp = get_no_body(&app, &authorize_uri).await;
    assert_eq!(
        resp.status(),
        StatusCode::SEE_OTHER,
        "authorize must redirect upstream"
    );
    let location = resp
        .headers()
        .get("location")
        .expect("Location header")
        .to_str()
        .unwrap()
        .to_owned();
    let upstream_url = url::Url::parse(&location).expect("upstream redirect is a URL");
    let upstream_state = query_param(&upstream_url, "state");
    let upstream_nonce = query_param(&upstream_url, "nonce");

    // Stand-in for "the browser completed the upstream provider's login".
    let code = provider.issue_code(&subject, Some("owner@example.com"), &upstream_nonce);

    let callback_uri =
        format!("/v1/oauth/{TEST_PROVIDER}/callback?code={code}&state={upstream_state}");
    let resp = get_no_body(&app, &callback_uri).await;
    assert_eq!(
        resp.status(),
        StatusCode::SEE_OTHER,
        "callback must redirect back to the client's redirect_uri"
    );
    let location = resp
        .headers()
        .get("location")
        .expect("Location header")
        .to_str()
        .unwrap()
        .to_owned();
    let client_redirect = url::Url::parse(&location).expect("client redirect is a URL");
    assert_eq!(client_redirect.scheme(), "axon");
    assert_eq!(query_param(&client_redirect, "state"), "client-state-abc");
    let axon_code = query_param(&client_redirect, "code");

    let (status, body) = post_form(
        &app,
        "/v1/oauth/token",
        &[
            ("grant_type", "authorization_code"),
            ("code", &axon_code),
            ("code_verifier", &verifier),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "token exchange failed: {body}");
    assert_eq!(body["token_type"], "Bearer");
    let access_token = body["access_token"]
        .as_str()
        .expect("access_token")
        .to_owned();
    let refresh_token = body["refresh_token"]
        .as_str()
        .expect("refresh_token")
        .to_owned();

    // The minted access token verifies on a real authed route — proves
    // `verify_token`'s new `expires_at` clause accepts a live OAuth token.
    let status = get_authed(&app, "/v1/accounts", &access_token).await;
    assert_eq!(status, StatusCode::OK);

    // Refresh rotation: redeeming the refresh token mints a new pair.
    let (status, body) = post_form(
        &app,
        "/v1/oauth/token",
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh_token),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "refresh failed: {body}");
    let new_access_token = body["access_token"]
        .as_str()
        .expect("access_token")
        .to_owned();
    assert_ne!(
        new_access_token, access_token,
        "refresh must mint a fresh access token"
    );

    // Reusing the now-rotated refresh token is rejected (reuse detection).
    let (status, body) = post_form(
        &app,
        "/v1/oauth/token",
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh_token),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");
}

#[tokio::test]
#[ignore]
async fn path_b_identity_token_grant_and_replay_rejection() {
    let store = store().await;
    let (app, provider) = app_with_oauth(store.clone());

    let subject = format!("test-subject-{}", Uuid::new_v4());
    store
        .bind_identity(TEST_PROVIDER, &subject, None)
        .await
        .expect("bind identity");

    let jti = format!("jti-path-b-{}", Uuid::new_v4());
    let identity_token = provider.sign_identity_token(&subject, None, None, Some(&jti));

    let (status, body) = post_form(
        &app,
        "/v1/oauth/token",
        &[
            ("grant_type", "urn:axon:identity_token"),
            ("provider", TEST_PROVIDER),
            ("identity_token", &identity_token),
            ("client_id", CLIENT_ID),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "identity token redemption failed: {body}"
    );
    assert!(body["access_token"].as_str().is_some());

    // The same identity token presented again is a replay — rejected even
    // though the signature/claims are still perfectly valid.
    let (status, body) = post_form(
        &app,
        "/v1/oauth/token",
        &[
            ("grant_type", "urn:axon:identity_token"),
            ("provider", TEST_PROVIDER),
            ("identity_token", &identity_token),
            ("client_id", CLIENT_ID),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");
}

#[tokio::test]
#[ignore]
async fn unbound_identity_is_rejected() {
    let store = store().await;
    let (app, provider) = app_with_oauth(store.clone());

    // No `bind_identity` call for this subject — Path B must refuse it.
    let subject = format!("never-bound-{}", Uuid::new_v4());
    let jti = format!("jti-unbound-{}", Uuid::new_v4());
    let identity_token = provider.sign_identity_token(&subject, None, None, Some(&jti));

    let (status, body) = post_form(
        &app,
        "/v1/oauth/token",
        &[
            ("grant_type", "urn:axon:identity_token"),
            ("provider", TEST_PROVIDER),
            ("identity_token", &identity_token),
            ("client_id", CLIENT_ID),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");
}

/// Regression: `/v1/oauth/token` must not be shadowed by the authed
/// sub-router's `/v1/{*path}` catch-all — it must return the RFC 6749 error
/// shape (proving `routes::oauth::token` handled it), not the authed
/// router's `401` (proving the bearer gate intercepted it first) or its
/// `404 route not found` (proving the catch-all won).
#[tokio::test]
#[ignore]
async fn oauth_token_route_is_not_shadowed_by_the_authed_catch_all() {
    let store = store().await;
    let (app, _provider) = app_with_oauth(store);
    let (status, body) = post_form(&app, "/v1/oauth/token", &[("grant_type", "bogus")]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "unsupported_grant_type");
}

/// Regression: a pre-existing CLI-minted token (`expires_at IS NULL`) must
/// still verify exactly as before OAuth existed.
#[tokio::test]
#[ignore]
async fn preexisting_cli_token_still_verifies() {
    let store = store().await;
    let (app, _provider) = app_with_oauth(store.clone());

    let issued = store
        .issue_token("cli-regression-check")
        .await
        .expect("issue token");
    let status = get_authed(&app, "/v1/accounts", &issued.token).await;
    assert_eq!(status, StatusCode::OK);
}

/// Minimal percent-encoding for a query-string value (this test only ever
/// encodes a fixed custom-scheme redirect URI, so a small local helper is
/// simpler than adding a dependency).
fn urlencoding_encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// The full `axon oauth bind` handshake (M14, ADR 0054): `GET
/// /v1/oauth/bind?user_code=...` redirects upstream keyed by the bind
/// request's own `device_code` (its `state`), and the callback binds a
/// **new** identity — the opposite of Path A, which requires one to already
/// exist. Then confirms that identity can immediately complete a real Path A
/// flow, closing the loop `unbound_identity_is_rejected` only proves the
/// negative of.
#[tokio::test]
#[ignore]
async fn bind_flow_creates_identity_then_path_a_succeeds() {
    let store = store().await;
    let (app, provider) = app_with_oauth(store.clone());

    let subject = format!("bind-subject-{}", Uuid::new_v4());
    let user_code = format!("TEST-{}", &Uuid::new_v4().simple().to_string()[..4]).to_uppercase();
    let bind_request = store
        .create_bind_request(
            TEST_PROVIDER,
            &user_code,
            Utc::now() + Duration::minutes(10),
        )
        .await
        .expect("create bind request");

    let resp = get_no_body(
        &app,
        &format!(
            "/v1/oauth/bind?user_code={}",
            urlencoding_encode(&user_code)
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::SEE_OTHER,
        "bind must redirect upstream"
    );
    let location = resp
        .headers()
        .get("location")
        .expect("Location header")
        .to_str()
        .unwrap()
        .to_owned();
    let upstream_url = url::Url::parse(&location).expect("upstream redirect is a URL");
    let upstream_state = query_param(&upstream_url, "state");
    let upstream_nonce = query_param(&upstream_url, "nonce");
    assert_eq!(
        upstream_state,
        format!("bind:{}", bind_request.device_code),
        "state must be the bind request's own device_code, explicitly tagged"
    );

    // Stand-in for "the admin completed the upstream provider's login".
    let code = provider.issue_code(&subject, Some("owner@example.com"), &upstream_nonce);
    let callback_uri =
        format!("/v1/oauth/{TEST_PROVIDER}/callback?code={code}&state={upstream_state}");
    let resp = get_no_body(&app, &callback_uri).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "bind callback should return a plain success page, not redirect anywhere"
    );

    let identity = store
        .find_identity(TEST_PROVIDER, &subject)
        .await
        .expect("query identity")
        .expect("identity must now be bound");
    assert_eq!(identity.email.as_deref(), Some("owner@example.com"));

    let completed = store
        .find_bind_request(bind_request.device_code)
        .await
        .expect("query bind request")
        .expect("bind request row must still exist");
    assert_eq!(completed.status, "completed");
    assert_eq!(completed.oauth_identity_id, Some(identity.id));

    // The identity bound above can now complete a real Path A flow — proves
    // the bind flow actually unblocks login, not just that it writes a row.
    let (verifier, challenge) = pkce_pair();
    let authorize_uri = format!(
        "/v1/oauth/authorize?response_type=code&client_id={CLIENT_ID}&redirect_uri={}&code_challenge={challenge}&code_challenge_method=S256&provider={TEST_PROVIDER}",
        urlencoding_encode(REDIRECT_URI),
    );
    let resp = get_no_body(&app, &authorize_uri).await;
    let location = resp
        .headers()
        .get("location")
        .expect("Location header")
        .to_str()
        .unwrap()
        .to_owned();
    let upstream_url = url::Url::parse(&location).expect("upstream redirect is a URL");
    let path_a_state = query_param(&upstream_url, "state");
    let path_a_nonce = query_param(&upstream_url, "nonce");
    let path_a_code = provider.issue_code(&subject, Some("owner@example.com"), &path_a_nonce);
    let resp = get_no_body(
        &app,
        &format!("/v1/oauth/{TEST_PROVIDER}/callback?code={path_a_code}&state={path_a_state}"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let client_redirect = url::Url::parse(
        resp.headers()
            .get("location")
            .expect("Location header")
            .to_str()
            .unwrap(),
    )
    .expect("client redirect is a URL");
    let axon_code = query_param(&client_redirect, "code");

    let (status, body) = post_form(
        &app,
        "/v1/oauth/token",
        &[
            ("grant_type", "authorization_code"),
            ("code", &axon_code),
            ("code_verifier", &verifier),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "token exchange failed: {body}");
}

/// A bind request's `device_code`/`state` can't be completed twice — the
/// second callback (a replay, or a slow duplicate request) must be rejected
/// rather than silently re-binding or overwriting `oauth_identity_id`.
#[tokio::test]
#[ignore]
async fn bind_request_cannot_be_completed_twice() {
    let store = store().await;
    let (app, provider) = app_with_oauth(store.clone());

    let subject = format!("bind-replay-{}", Uuid::new_v4());
    let user_code = Uuid::new_v4().simple().to_string()[..9].to_uppercase();
    let bind_request = store
        .create_bind_request(
            TEST_PROVIDER,
            &user_code,
            Utc::now() + Duration::minutes(10),
        )
        .await
        .expect("create bind request");

    let resp = get_no_body(
        &app,
        &format!(
            "/v1/oauth/bind?user_code={}",
            urlencoding_encode(&user_code)
        ),
    )
    .await;
    let upstream_url =
        url::Url::parse(resp.headers().get("location").unwrap().to_str().unwrap()).unwrap();
    let upstream_state = query_param(&upstream_url, "state");
    let upstream_nonce = query_param(&upstream_url, "nonce");

    let code = provider.issue_code(&subject, None, &upstream_nonce);
    let callback_uri =
        format!("/v1/oauth/{TEST_PROVIDER}/callback?code={code}&state={upstream_state}");
    let resp = get_no_body(&app, &callback_uri).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Replaying the same `device_code`/`state` after completion: the
    // bind-flow branch's lookup only matches a `pending` row (mirroring Path
    // A's `find_authorization_request_by_upstream_state`, which is likewise
    // scoped to `status = 'pending'`), so a fully-sequential replay after
    // completion falls through and 400s as "unknown" — the same outcome
    // Path A gives a stale `state`. `complete_bind_request`'s atomic
    // conditional `UPDATE` (409 on a lost race) is what actually guards the
    // *concurrent* case, where two requests both observe `pending` before
    // either's completion lands; a sharper fresh code bound to the same
    // `device_code` still proves the row can't be driven through twice.
    let code2 = provider.issue_code(&subject, None, &upstream_nonce);
    let callback_uri2 =
        format!("/v1/oauth/{TEST_PROVIDER}/callback?code={code2}&state={upstream_state}");
    let resp = get_no_body(&app, &callback_uri2).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "a second completion of the same bind request must be rejected"
    );

    let final_state = store
        .find_bind_request(bind_request.device_code)
        .await
        .expect("query bind request")
        .expect("row still exists");
    assert_eq!(final_state.status, "completed");
}

/// Regression: `complete_bind_request` must not write an identity for a row
/// that's already `expires_at`-expired, even though it's still `status =
/// 'pending'` (nothing has swept it yet). An earlier version of this code
/// let `bind_identity`'s UPSERT run unconditionally *before* the only
/// expiry-checked step, so a stale/replayed callback could permanently bind
/// an identity despite the caller seeing a `409` rejection.
#[tokio::test]
#[ignore]
async fn expired_bind_request_does_not_bind_an_identity() {
    let store = store().await;
    let subject = format!("expired-bind-{}", Uuid::new_v4());
    let user_code = Uuid::new_v4().simple().to_string()[..9].to_uppercase();

    let request = store
        .create_bind_request(TEST_PROVIDER, &user_code, Utc::now() - Duration::minutes(1))
        .await
        .expect("create already-expired bind request");

    let identity_id = store
        .complete_bind_request(request.device_code, TEST_PROVIDER, &subject, None)
        .await
        .expect("query complete_bind_request");
    assert_eq!(
        identity_id, None,
        "an expired bind request must not be completable"
    );

    let identity = store
        .find_identity(TEST_PROVIDER, &subject)
        .await
        .expect("query identity");
    assert!(
        identity.is_none(),
        "no identity should have been written for an expired bind request"
    );
}

/// Regression: `axon oauth identities unbind` (revoke tokens, then delete the
/// identity) must succeed even for an identity that was actually bound
/// through `axon oauth bind` — a completed bind request's `oauth_identity_id`
/// FK previously had no `ON DELETE` action, so `delete_identity` failed with
/// a foreign-key violation for exactly this case.
#[tokio::test]
#[ignore]
async fn unbind_succeeds_for_an_identity_bound_via_the_bind_flow() {
    let store = store().await;
    let subject = format!("unbind-after-bind-{}", Uuid::new_v4());
    let user_code = Uuid::new_v4().simple().to_string()[..9].to_uppercase();

    let request = store
        .create_bind_request(
            TEST_PROVIDER,
            &user_code,
            Utc::now() + Duration::minutes(10),
        )
        .await
        .expect("create bind request");
    let identity_id = store
        .complete_bind_request(request.device_code, TEST_PROVIDER, &subject, None)
        .await
        .expect("complete bind request")
        .expect("bind request must complete");

    // Mirrors `axon oauth identities unbind`'s own sequence.
    store
        .revoke_tokens_for_identity(identity_id)
        .await
        .expect("revoke tokens");
    store
        .revoke_refresh_tokens_for_identity(identity_id)
        .await
        .expect("revoke refresh tokens");
    let deleted = store
        .delete_identity(identity_id)
        .await
        .expect("delete identity must not fail with a foreign-key violation");
    assert!(deleted);
}
