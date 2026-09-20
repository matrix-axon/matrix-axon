use super::*;
use axum::{
    routing::{get, post},
    Form, Json, Router,
};
use jsonwebtoken::{decode, DecodingKey, Validation};
use serde_json::{json, Value};
use std::collections::HashMap;

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/common/signing_key.rs"
));
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/common/apple_rsa_key.rs"
));

fn config() -> AppleOauthConfig {
    AppleOauthConfig {
        client_id: Some("com.example.web".into()),
        native_audiences: vec!["com.example.ios".into()],
        team_id: Some("TEAM123456".into()),
        key_id: Some(TEST_KID.into()),
        ..Default::default()
    }
}

fn provider() -> AppleProvider {
    AppleProvider::new(
        super::super::http_client(),
        &config(),
        ec_key().pem.as_bytes(),
    )
    .unwrap()
}

async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}"), task)
}

async fn verification_provider() -> (AppleProvider, tokio::task::JoinHandle<()>) {
    let (base, task) = serve(Router::new().route("/keys", get(|| async {
        Json(json!({"keys": [{"kty":"RSA", "kid":"apple-test", "alg":"RS256", "use":"sig", "n":&rsa_key().n, "e":&rsa_key().e}]}))
    }))).await;
    let mut provider = provider();
    provider.jwks = JwksCache::new(super::super::http_client(), format!("{base}/keys"));
    (provider, task)
}

fn claims(audience: &str) -> Value {
    let now = Utc::now().timestamp();
    json!({"iss":ISSUER, "aud":audience, "sub":"owner", "iat":now, "exp":now+300, "nonce":"expected", "email":"relay@privaterelay.appleid.com"})
}

fn sign(claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("apple-test".into());
    encode(
        &header,
        claims,
        &EncodingKey::from_rsa_pem(rsa_key().pem.as_bytes()).unwrap(),
    )
    .unwrap()
}

fn verify_secret(secret: &str) -> Value {
    let mut validation = Validation::new(Algorithm::ES256);
    validation.set_audience(&[ISSUER]);
    validation.set_issuer(&["TEAM123456"]);
    decode::<Value>(
        secret,
        &DecodingKey::from_ec_components(&ec_key().x, &ec_key().y).unwrap(),
        &validation,
    )
    .unwrap()
    .claims
}

#[test]
fn authorization_requests_code_form_post_and_encodes_state() {
    let provider = provider();
    let url = url::Url::parse(&provider.authorize_url(
        "a&b=?",
        "nonce",
        "https://axon.example/v1/oauth/apple/callback",
    ))
    .unwrap();
    assert_eq!(url.origin().ascii_serialization(), ISSUER);
    let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["response_mode"], "form_post");
    assert_eq!(params["scope"], "email");
    assert_eq!(params["state"], "a&b=?");
    assert_eq!(params["nonce"], "nonce");
    assert_eq!(params["client_id"], "com.example.web");
}

#[test]
fn client_secret_is_signed_for_apple_and_renews_without_a_timer() {
    let provider = provider();
    let now = Utc::now().timestamp();
    for issued in [now, now + 86400 * 365] {
        let secret = provider.client_secret(issued).unwrap();
        let header = jsonwebtoken::decode_header(&secret).unwrap();
        assert_eq!(header.alg, Algorithm::ES256);
        assert_eq!(header.kid.as_deref(), Some(TEST_KID));
        let claims = verify_secret(&secret);
        assert_eq!(claims["sub"], "com.example.web");
        assert_eq!(claims["iat"], issued - 60);
        assert_eq!(claims["exp"], issued + 300);
        // Simulate Apple's clock lagging this host by up to one minute.
        for ahead_by in [0, 1, 30, 60] {
            let apple_now = issued - ahead_by;
            assert!(claims["iat"].as_i64().unwrap() <= apple_now);
            assert!(claims["exp"].as_i64().unwrap() > apple_now);
        }
    }
}

#[test]
fn bad_configuration_fails_without_disclosing_key_material() {
    let error = AppleProvider::new(super::super::http_client(), &config(), b"PRIVATE_SENTINEL")
        .err()
        .unwrap();
    assert!(!error.to_string().contains("PRIVATE_SENTINEL"));
    for field in ["client_id", "team_id", "key_id", "native_audiences"] {
        let mut cfg = config();
        match field {
            "client_id" => cfg.client_id = None,
            "team_id" => cfg.team_id = Some(String::new()),
            "key_id" => cfg.key_id = Some(" ".into()),
            _ => cfg.native_audiences.push(String::new()),
        }
        assert!(
            AppleProvider::new(super::super::http_client(), &cfg, ec_key().pem.as_bytes()).is_err()
        );
    }
}

#[tokio::test]
async fn signed_apple_tokens_keep_web_and_native_audiences_separate() {
    let (provider, task) = verification_provider().await;
    let web = sign(&claims("com.example.web"));
    let native = sign(&claims("com.example.ios"));
    assert!(provider
        .verify_identity_token(&web, Some(""))
        .await
        .is_err());
    assert!(provider
        .verify_native_identity_token(&native, "")
        .await
        .is_err());
    let verified = provider
        .verify_identity_token(&web, Some("expected"))
        .await
        .unwrap();
    assert_eq!(verified.subject, "owner");
    assert_eq!(
        verified.email.as_deref(),
        Some("relay@privaterelay.appleid.com")
    );
    assert!(!verified.replay_key.is_empty());
    assert_ne!(verified.replay_key, web);
    assert_eq!(
        verified.replay_key,
        provider
            .verify_identity_token(&web, Some("expected"))
            .await
            .unwrap()
            .replay_key
    );
    assert!(provider
        .verify_native_identity_token(&native, "expected")
        .await
        .is_ok());
    assert!(matches!(
        provider
            .verify_native_identity_token(&web, "expected")
            .await,
        Err(OidcError::InvalidAudience(_))
    ));
    assert!(matches!(
        provider
            .verify_identity_token(&native, Some("expected"))
            .await,
        Err(OidcError::InvalidAudience(_))
    ));
    assert!(matches!(
        provider.verify_identity_token(&native, None).await,
        Err(OidcError::InvalidNonce)
    ));
    task.abort();
}

#[tokio::test]
async fn rejects_invalid_signed_claims_and_allows_repeat_login_without_profile() {
    let (provider, task) = verification_provider().await;
    let now = Utc::now().timestamp();
    for (field, value) in [
        ("iss", json!("PRIVATE_SENTINEL")),
        ("aud", json!("PRIVATE_SENTINEL")),
        ("exp", json!(now - 120)),
        ("iat", json!(now + 120)),
        ("nbf", json!(now + 120)),
        ("nonce", json!("wrong")),
        ("nonce", Value::Null),
        ("sub", json!("")),
        ("exp", json!(i64::MIN)),
    ] {
        let mut invalid = claims("com.example.web");
        invalid[field] = value;
        let error = provider
            .verify_identity_token(&sign(&invalid), Some("expected"))
            .await
            .expect_err(field);
        assert!(!error.to_string().contains("PRIVATE_SENTINEL"));
    }
    let mut returning = claims("com.example.web");
    returning.as_object_mut().unwrap().remove("email");
    assert!(provider
        .verify_identity_token(&sign(&returning), Some("expected"))
        .await
        .unwrap()
        .email
        .is_none());
    let mut forged: Vec<String> = sign(&returning).split('.').map(str::to_owned).collect();
    let replacement = if forged[2].starts_with('a') { "b" } else { "a" };
    forged[2].replace_range(..1, replacement);
    assert!(provider
        .verify_identity_token(&forged.join("."), Some("expected"))
        .await
        .is_err());
    assert!(provider
        .verify_identity_token(&"x".repeat(16385), Some("expected"))
        .await
        .is_err());
    let mut unknown_header = Header::new(Algorithm::RS256);
    unknown_header.kid = Some("PRIVATE_SENTINEL".into());
    let unknown = encode(
        &unknown_header,
        &returning,
        &EncodingKey::from_rsa_pem(rsa_key().pem.as_bytes()).unwrap(),
    )
    .unwrap();
    let error = provider
        .verify_identity_token(&unknown, Some("expected"))
        .await
        .err()
        .unwrap();
    assert!(!error.to_string().contains("PRIVATE_SENTINEL"));
    let mut wrong_algorithm = Header::new(Algorithm::HS256);
    wrong_algorithm.kid = Some("apple-test".into());
    let hmac = encode(
        &wrong_algorithm,
        &returning,
        &EncodingKey::from_secret(b"not-an-apple-key"),
    )
    .unwrap();
    assert!(matches!(
        provider
            .verify_identity_token(&hmac, Some("expected"))
            .await,
        Err(OidcError::DisallowedAlgorithm(_))
    ));
    task.abort();
}

#[tokio::test]
async fn code_exchange_posts_a_verified_client_secret_and_keeps_upstream_tokens_internal() {
    let (base, task) = serve(Router::new().route("/token", post(|Form(form): Form<HashMap<String, String>>| async move {
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["code"], "code&with=special");
        assert_eq!(form["redirect_uri"], "https://axon.example/callback");
        assert_eq!(verify_secret(&form["client_secret"])["sub"], form["client_id"]);
        Json(json!({"id_token":"signed-token", "access_token":"discard", "refresh_token":"discard"}))
    }))).await;
    let mut provider = provider();
    provider.token_url = format!("{base}/token");
    assert_eq!(
        provider
            .exchange_code("code&with=special", "https://axon.example/callback")
            .await
            .unwrap()
            .id_token,
        "signed-token"
    );
    task.abort();
}

#[tokio::test]
async fn exchange_failures_are_bounded_and_redacted() {
    let router = Router::new()
        .route(
            "/denied",
            post(|| async { (axum::http::StatusCode::BAD_REQUEST, "PRIVATE_SENTINEL") }),
        )
        .route("/malformed", post(|| async { "PRIVATE_SENTINEL" }))
        .route(
            "/missing",
            post(|| async { Json(json!({"access_token":"PRIVATE_SENTINEL"})) }),
        )
        .route(
            "/large",
            post(|| async { "x".repeat(super::super::MAX_HTTP_RESPONSE_BYTES + 1) }),
        )
        .route(
            "/slow",
            post(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                "too late"
            }),
        );
    let (base, task) = serve(router).await;
    let mut provider = provider();
    provider.http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(100))
        .build()
        .unwrap();
    for path in ["denied", "malformed", "missing", "large", "slow"] {
        provider.token_url = format!("{base}/{path}");
        let error = provider
            .exchange_code("SECRET_CODE", "https://axon.example/callback")
            .await
            .expect_err(path);
        assert!(!error.to_string().contains("PRIVATE_SENTINEL"));
        assert!(!error.to_string().contains("SECRET_CODE"));
    }
    task.abort();
}

#[tokio::test]
async fn transport_categories_survive_exchange_and_jwks_verification() {
    use axum::{body::Body, http::StatusCode, response::Response};
    use futures_util::{stream, StreamExt};
    use std::{convert::Infallible, time::Duration};

    let (base, task) = serve(
        Router::new()
            .route(
                "/denied",
                get(|| async { (StatusCode::SERVICE_UNAVAILABLE, "PRIVATE_SENTINEL") })
                    .post(|| async { (StatusCode::SERVICE_UNAVAILABLE, "PRIVATE_SENTINEL") }),
            )
            .route(
                "/slow",
                get(|| async { std::future::pending::<String>().await })
                    .post(|| async { std::future::pending::<String>().await }),
            )
            .route(
                "/slow-body",
                get(|| async {
                    Response::new(Body::from_stream(
                        stream::iter([Ok::<_, Infallible>("{")]).chain(stream::pending()),
                    ))
                })
                .post(|| async {
                    Response::new(Body::from_stream(
                        stream::iter([Ok::<_, Infallible>("{")]).chain(stream::pending()),
                    ))
                }),
            ),
    )
    .await;
    // A bound, non-listening socket deterministically refuses connections
    // without racing another test for a freshly released port.
    let closed = tokio::net::TcpSocket::new_v4().unwrap();
    closed.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let refused = format!("http://{}/PRIVATE_SENTINEL", closed.local_addr().unwrap());
    let mut provider = provider();
    provider.http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(250))
        .build()
        .unwrap();
    let token = sign(&claims("com.example.web"));
    for (url, category) in [
        (format!("{base}/denied?secret=PRIVATE_SENTINEL"), "503"),
        (format!("{base}/slow?secret=PRIVATE_SENTINEL"), "timeout"),
        (
            format!("{base}/slow-body?secret=PRIVATE_SENTINEL"),
            "timeout",
        ),
        (refused, "connect (including DNS/TLS)"),
        ("invalid-url-PRIVATE_SENTINEL".into(), "transport"),
    ] {
        provider.token_url = url.clone();
        provider.jwks = JwksCache::new(provider.http.clone(), url);
        let exchange = provider
            .exchange_code("SECRET_CODE", "https://axon.example/callback")
            .await
            .unwrap_err();
        let verification = provider
            .verify_identity_token(&token, Some("expected"))
            .await
            .unwrap_err();
        for error in [exchange, verification] {
            assert!(matches!(error, OidcError::Http(_)));
            let message = error.to_string();
            assert!(message.contains(category), "{message}");
            for secret in [
                "PRIVATE_SENTINEL",
                "SECRET_CODE",
                token.as_str(),
                base.as_str(),
            ] {
                assert!(!message.contains(secret));
            }
        }
    }
    task.abort();
}
