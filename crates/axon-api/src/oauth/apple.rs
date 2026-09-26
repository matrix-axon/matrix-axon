//! Sign in with Apple provider foundation (ADR 0054).
//!
//! Registered for credentialed browser login. Native login remains gated until
//! the server-issued challenge and native owner-binding contract lands.

use axon_core::AppleOauthConfig;
use chrono::Utc;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;

use super::jwks::JwksCache;
use super::{OidcError, OidcProvider, UpstreamTokens, VerifiedIdentity};

const ISSUER: &str = "https://appleid.apple.com";
const AUTHORIZE_URL: &str = "https://appleid.apple.com/auth/authorize";
const TOKEN_URL: &str = "https://appleid.apple.com/auth/token";
const JWKS_URL: &str = "https://appleid.apple.com/auth/keys";
// Sign on demand, so idle servers never retain an expired client secret and
// no timer, cache lock, or background task is needed. Apple's limit is six months.
const CLIENT_SECRET_TTL_SECS: i64 = 300;

#[cfg(test)]
mod tests;

/// Apple credentials stay server-side. This type deliberately has no Debug
/// implementation: neither the signing key nor generated JWTs are diagnostics.
pub struct AppleProvider {
    http: reqwest::Client,
    client_id: String,
    native_audiences: Vec<String>,
    team_id: String,
    key_id: String,
    signing_key: EncodingKey,
    jwks: JwksCache,
    token_url: String,
}

#[derive(Serialize)]
struct ClientSecret<'a> {
    iss: &'a str,
    sub: &'a str,
    aud: &'a str,
    iat: i64,
    exp: i64,
}

impl AppleProvider {
    /// Construct the browser provider without fetching Apple's endpoints.
    /// The caller loads PEM bytes from protected configuration or a key file;
    /// errors never include their contents. Use the bounded OAuth HTTP client.
    pub fn new(
        http: reqwest::Client,
        config: &AppleOauthConfig,
        private_key_pem: &[u8],
    ) -> Result<Self, OidcError> {
        let client_id = required(config.client_id.as_deref(), "client_id")?;
        let team_id = required(config.team_id.as_deref(), "team_id")?;
        let key_id = required(config.key_id.as_deref(), "key_id")?;
        if config
            .native_audiences
            .iter()
            .any(|aud| aud.trim().is_empty())
        {
            return Err(OidcError::Malformed("empty Apple native audience".into()));
        }
        let signing_key = EncodingKey::from_ec_pem(private_key_pem)
            .map_err(|_| OidcError::Malformed("invalid Apple ES256 private key".into()))?;
        let provider = Self {
            jwks: JwksCache::new(http.clone(), JWKS_URL.into()),
            http,
            client_id: client_id.into(),
            native_audiences: config.native_audiences.clone(),
            team_id: team_id.into(),
            key_id: key_id.into(),
            signing_key,
            token_url: TOKEN_URL.into(),
        };
        // Parsing PEM alone need not validate the curve or usable key material.
        // Prove ES256 signing works now, before accepting any login attempt.
        provider.client_secret(Utc::now().timestamp())?;
        Ok(provider)
    }

    fn client_secret(&self, now: i64) -> Result<String, OidcError> {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(self.key_id.clone());
        encode(
            &header,
            &ClientSecret {
                iss: &self.team_id,
                sub: &self.client_id,
                aud: ISSUER,
                // Allow a slightly fast host clock without extending expiry.
                iat: now.saturating_sub(super::verification::CLOCK_SKEW_SECS),
                exp: now.saturating_add(CLIENT_SECRET_TTL_SECS),
            },
            &self.signing_key,
        )
        .map_err(|_| OidcError::Malformed("Apple client-secret signing failed".into()))
    }

    /// Native tokens use explicitly configured bundle IDs, never the web
    /// Services ID. The future native handler must obtain this nonce from its
    /// own single-use challenge record, not from a client-provided expectation.
    pub async fn verify_native_identity_token(
        &self,
        token: &str,
        nonce: &str,
    ) -> Result<VerifiedIdentity, OidcError> {
        if nonce.is_empty() {
            return Err(OidcError::InvalidNonce);
        }
        let audiences: Vec<&str> = self.native_audiences.iter().map(String::as_str).collect();
        super::verification::verify(&self.jwks, token, ISSUER, &audiences, Some(nonce)).await
    }
}

fn required<'a>(value: Option<&'a str>, field: &str) -> Result<&'a str, OidcError> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| OidcError::Malformed(format!("oauth.providers.apple.{field} is required")))
}

#[async_trait::async_trait]
impl OidcProvider for AppleProvider {
    fn is_cancellation(&self, error: &str) -> bool {
        matches!(error, "access_denied" | "user_cancelled_authorize")
    }

    fn name(&self) -> &'static str {
        "apple"
    }

    fn authorize_url(&self, state: &str, nonce: &str, redirect_uri: &str) -> String {
        let mut url = url::Url::parse(AUTHORIZE_URL).expect("static Apple authorize URL");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("response_mode", "form_post")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", "email")
            .append_pair("state", state)
            .append_pair("nonce", nonce);
        url.into()
    }

    async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
    ) -> Result<UpstreamTokens, OidcError> {
        let secret = self.client_secret(Utc::now().timestamp())?;
        super::exchange::authorization_code(
            &self.http,
            &self.token_url,
            &self.client_id,
            &secret,
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
        // Do not accidentally enable Apple's native grant through the legacy
        // nonce-free Path B handler. Native support needs a server challenge.
        let nonce = nonce
            .filter(|nonce| !nonce.is_empty())
            .ok_or(OidcError::InvalidNonce)?;
        super::verification::verify(&self.jwks, token, ISSUER, &[&self.client_id], Some(nonce))
            .await
    }
}
