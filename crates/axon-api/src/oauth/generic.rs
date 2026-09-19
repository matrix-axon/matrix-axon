//! `GenericOidcProvider`: the discovery-doc-driven [`OidcProvider`] impl
//! shared by Google and Microsoft (M14b, ADR 0054).
//!
//! Both are plain OIDC providers: fetch `.well-known/openid-configuration`
//! once at construction (cached for the provider's lifetime), then drive
//! authorize/token-exchange/verify off the endpoints it publishes. Signature
//! verification is [`jsonwebtoken`]'s job; issuer/audience/nonce/expiry
//! checks are done by hand here rather than through that crate's built-in
//! validator, so the one genuinely provider-specific wrinkle — Microsoft's
//! multi-tenant `{tenantid}`-templated issuer — has a clear, testable seam
//! ([`super::verification::issuer_matches`]) instead of fighting a generic library's assumptions.

use serde::Deserialize;

use crate::oauth::jwks::JwksCache;
use crate::oauth::provider::{OidcError, OidcProvider, UpstreamTokens, VerifiedIdentity};
use crate::oauth::{read_json_capped, MAX_HTTP_RESPONSE_BYTES};

/// A discovery-doc-driven OIDC provider (Google, Microsoft).
pub struct GenericOidcProvider {
    name: &'static str,
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    /// Parsed once at [`discover`](Self::discover) time, so a malformed
    /// provider-published endpoint fails discovery with an [`OidcError`]
    /// rather than panicking later on the request path, where
    /// [`authorize_url`](OidcProvider::authorize_url) has no `Result` to
    /// return one through.
    authorization_endpoint: url::Url,
    discovery: DiscoveryDocument,
    jwks: JwksCache,
}

#[derive(Debug, Clone, Deserialize)]
struct DiscoveryDocument {
    /// The provider's own asserted issuer. For Microsoft's multi-tenant
    /// endpoints (`common`/`organizations`/`consumers`) this is a template
    /// containing the literal string `{tenantid}` — see [`issuer_matches`].
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

impl GenericOidcProvider {
    /// Fetch `issuer`'s discovery document once and build a provider over it.
    /// The `.well-known/openid-configuration` path is appended directly to
    /// `issuer` (not RFC 5785's host-first insertion) — this matches how
    /// Microsoft's own multi-tenant issuers (which carry a path, e.g.
    /// `.../common/v2.0`) actually publish discovery, and is a no-op
    /// distinction for Google's path-free issuer.
    pub async fn discover(
        name: &'static str,
        http: reqwest::Client,
        issuer: &str,
        client_id: String,
        client_secret: String,
    ) -> Result<Self, OidcError> {
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let response = http
            .get(&discovery_url)
            .send()
            .await
            .map_err(|err| OidcError::Http(err.to_string()))?;
        if !response.status().is_success() {
            return Err(OidcError::Http(format!(
                "discovery fetch from {discovery_url} returned {}",
                response.status()
            )));
        }
        let discovery: DiscoveryDocument =
            read_json_capped(response, MAX_HTTP_RESPONSE_BYTES).await?;

        // Discovery documents are remote, provider-published input — validate
        // every endpoint URL now, while a failure can still surface as a
        // clean boot-time/construction error, rather than later on a request
        // path that has no clean way to reject a malformed one.
        let authorization_endpoint =
            url::Url::parse(&discovery.authorization_endpoint).map_err(|err| {
                OidcError::Malformed(format!(
                    "discovery document's authorization_endpoint {:?} is not a valid URL: {err}",
                    discovery.authorization_endpoint
                ))
            })?;
        url::Url::parse(&discovery.token_endpoint).map_err(|err| {
            OidcError::Malformed(format!(
                "discovery document's token_endpoint {:?} is not a valid URL: {err}",
                discovery.token_endpoint
            ))
        })?;
        url::Url::parse(&discovery.jwks_uri).map_err(|err| {
            OidcError::Malformed(format!(
                "discovery document's jwks_uri {:?} is not a valid URL: {err}",
                discovery.jwks_uri
            ))
        })?;

        let jwks = JwksCache::new(http.clone(), discovery.jwks_uri.clone());
        Ok(Self {
            name,
            http,
            client_id,
            client_secret,
            authorization_endpoint,
            discovery,
            jwks,
        })
    }
}

#[async_trait::async_trait]
impl OidcProvider for GenericOidcProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn authorize_url(&self, state: &str, nonce: &str, redirect_uri: &str) -> String {
        let mut url = self.authorization_endpoint.clone();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", "openid email")
            .append_pair("state", state)
            .append_pair("nonce", nonce);
        url.to_string()
    }

    async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
    ) -> Result<UpstreamTokens, OidcError> {
        super::exchange::authorization_code(
            &self.http,
            &self.discovery.token_endpoint,
            &self.client_id,
            &self.client_secret,
            code,
            redirect_uri,
        )
        .await
    }

    async fn verify_identity_token(
        &self,
        token: &str,
        nonce: Option<&str>,
    ) -> Result<VerifiedIdentity, OidcError> {
        super::verification::verify(
            &self.jwks,
            token,
            &self.discovery.issuer,
            &[&self.client_id],
            nonce,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::verification::{audience_contains, issuer_matches};
    use axum::routing::get;
    use axum::{Json, Router};
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use serde_json::{json, Value};

    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/common/signing_key.rs"
    ));

    #[tokio::test]
    async fn real_signed_google_and_microsoft_tokens_use_the_shared_verifier() {
        for (name, issuer, token_issuer, tid) in [
            (
                "google",
                "https://accounts.google.com",
                "https://accounts.google.com",
                None,
            ),
            (
                "microsoft",
                "https://login.microsoftonline.com/{tenantid}/v2.0",
                "https://login.microsoftonline.com/test-tenant/v2.0",
                Some("test-tenant"),
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let document = json!({"issuer":issuer, "authorization_endpoint":format!("{base}/authorize"), "token_endpoint":format!("{base}/token"), "jwks_uri":format!("{base}/keys")});
            let router = Router::new()
                .route("/.well-known/openid-configuration", get(move || { let document = document.clone(); async move { Json(document) } }))
                .route("/keys", get(|| async { Json(json!({"keys":[{"kty":"EC", "crv":"P-256", "kid":TEST_KID, "x":TEST_EC_X, "y":TEST_EC_Y}]})) }));
            let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let provider = GenericOidcProvider::discover(
                name,
                crate::oauth::http_client(),
                &base,
                "client".into(),
                "secret".into(),
            )
            .await
            .unwrap();
            let now = chrono::Utc::now().timestamp();
            let mut claims = json!({"iss":token_issuer, "aud":["other", "client"], "sub":"owner", "iat":now, "exp":now+300, "tid":tid, "nonce":"n", "jti":"replay"});
            let mut header = Header::new(Algorithm::ES256);
            header.kid = Some(TEST_KID.into());
            let key = EncodingKey::from_ec_pem(TEST_EC_PRIVATE_KEY_PEM.as_bytes()).unwrap();
            let token = encode(&header, &claims, &key).unwrap();
            assert_eq!(
                provider
                    .verify_identity_token(&token, Some("n"))
                    .await
                    .unwrap()
                    .replay_key,
                "replay"
            );
            assert!(provider.verify_identity_token(&token, None).await.is_ok());
            assert!(matches!(
                provider.verify_identity_token(&token, Some("wrong")).await,
                Err(OidcError::InvalidNonce)
            ));
            claims["iss"] = json!("https://wrong.example");
            let invalid = encode(&header, &claims, &key).unwrap();
            assert!(matches!(
                provider.verify_identity_token(&invalid, None).await,
                Err(OidcError::InvalidIssuer(_))
            ));
            task.abort();
        }
    }
    async fn serve(router: Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        addr
    }

    #[tokio::test]
    async fn discover_rejects_a_malformed_authorization_endpoint() {
        let addr = Router::new().route(
            "/.well-known/openid-configuration",
            get(|| async {
                Json(json!({
                    "issuer": "https://issuer.example.com",
                    "authorization_endpoint": "not a valid url",
                    "token_endpoint": "https://issuer.example.com/token",
                    "jwks_uri": "https://issuer.example.com/jwks",
                }))
            }),
        );
        let addr = serve(addr).await;
        let result = GenericOidcProvider::discover(
            "test",
            reqwest::Client::new(),
            &format!("http://{addr}"),
            "client".to_owned(),
            "secret".to_owned(),
        )
        .await;
        let err = match result {
            Err(err) => err,
            Ok(_) => {
                panic!("a malformed authorization_endpoint must fail discovery, not panic later")
            }
        };
        assert!(matches!(err, OidcError::Malformed(_)));
    }

    #[test]
    fn plain_issuer_requires_exact_match() {
        assert!(issuer_matches(
            "https://accounts.google.com",
            "https://accounts.google.com",
            None
        ));
        assert!(!issuer_matches(
            "https://accounts.google.com",
            "https://evil.example.com",
            None
        ));
    }

    #[test]
    fn templated_issuer_substitutes_the_tokens_own_tenant_id() {
        let configured = "https://login.microsoftonline.com/{tenantid}/v2.0";
        let token_iss =
            "https://login.microsoftonline.com/9188040d-6c67-4c5b-b112-36a304b66dad/v2.0";
        assert!(issuer_matches(
            configured,
            token_iss,
            Some("9188040d-6c67-4c5b-b112-36a304b66dad")
        ));
    }

    #[test]
    fn templated_issuer_rejects_a_mismatched_tenant() {
        let configured = "https://login.microsoftonline.com/{tenantid}/v2.0";
        let token_iss =
            "https://login.microsoftonline.com/aaaaaaaa-0000-0000-0000-000000000000/v2.0";
        assert!(!issuer_matches(
            configured,
            token_iss,
            Some("9188040d-6c67-4c5b-b112-36a304b66dad")
        ));
    }

    #[test]
    fn templated_issuer_without_a_tid_claim_is_rejected() {
        // A token that doesn't carry `tid` can't satisfy a templated issuer —
        // there's nothing to substitute, so this must fail closed, not match
        // the literal template string.
        let configured = "https://login.microsoftonline.com/{tenantid}/v2.0";
        assert!(!issuer_matches(configured, configured, None));
    }

    #[test]
    fn audience_matches_bare_string_or_array() {
        assert!(audience_contains(&Value::String("abc".into()), "abc"));
        assert!(!audience_contains(&Value::String("abc".into()), "xyz"));
        assert!(audience_contains(
            &Value::Array(vec![
                Value::String("one".into()),
                Value::String("abc".into())
            ]),
            "abc"
        ));
        assert!(!audience_contains(&Value::Null, "abc"));
    }
}
