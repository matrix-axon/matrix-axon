//! A fake [`OidcProvider`] for the M14b OAuth integration test.
//!
//! Signs and verifies real ES256-signed JWTs (so the id_token plumbing —
//! signature verification, `iss`/`aud`/`nonce`/`exp` checks — is genuinely
//! exercised), but stands in for the network calls a real provider would
//! need: [`issue_code`](TestOidcProvider::issue_code) is the test's stand-in
//! for "the browser completed the upstream provider's own login page", and
//! [`sign_identity_token`](TestOidcProvider::sign_identity_token) is the
//! stand-in for "the native SDK handed the app a signed identity token"
//! (Path B).
//!
//! Each test process generates its own P-256 keypair in memory.
//! No private signing material is committed or persisted.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use axon_api::{OidcError, OidcProvider, UpstreamTokens, VerifiedIdentity};
use chrono::Utc;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use axon_test_support::{ec_key, TEST_KID};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TestClaims {
    iss: String,
    aud: String,
    sub: String,
    exp: i64,
    iat: i64,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    jti: Option<String>,
}

struct PendingCode {
    sub: String,
    email: Option<String>,
    nonce: String,
}

/// A fake OIDC provider: signs and verifies real ES256 JWTs against a
/// throwaway test keypair, standing in for a real upstream provider's
/// network calls.
pub struct TestOidcProvider {
    name: &'static str,
    issuer: String,
    audience: String,
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
    codes: Mutex<HashMap<String, PendingCode>>,
}

impl TestOidcProvider {
    /// Build a fake provider asserting the given `issuer`/`audience` in every
    /// token it signs (mirroring a real provider's discovery-doc issuer and
    /// axon's registered `client_id` with it).
    pub fn new(name: &'static str, issuer: &str, audience: &str) -> Self {
        Self {
            name,
            issuer: issuer.to_owned(),
            audience: audience.to_owned(),
            encoding_key: EncodingKey::from_ec_pem(ec_key().pem.as_bytes())
                .expect("test EC key must parse"),
            decoding_key: DecodingKey::from_ec_components(&ec_key().x, &ec_key().y)
                .expect("test EC coordinates must parse"),
            codes: Mutex::new(HashMap::new()),
        }
    }

    /// The test's stand-in for "the browser completed the upstream
    /// provider's login page": register a pending authorization bound to
    /// `nonce` (extracted from the `authorize_url` this provider returned)
    /// and return an opaque code for the callback handler to exchange.
    pub fn issue_code(&self, sub: &str, email: Option<&str>, nonce: &str) -> String {
        let code = format!("test-code-{}", Uuid::new_v4());
        self.codes.lock().unwrap().insert(
            code.clone(),
            PendingCode {
                sub: sub.to_owned(),
                email: email.map(str::to_owned),
                nonce: nonce.to_owned(),
            },
        );
        code
    }

    /// The test's stand-in for a native SDK handing the app a signed
    /// identity token directly (Path B) — no authorization code, no nonce.
    pub fn sign_identity_token(
        &self,
        sub: &str,
        email: Option<&str>,
        nonce: Option<&str>,
        jti: Option<&str>,
    ) -> String {
        self.sign(sub, email, nonce, jti, 3600)
    }

    /// As [`sign_identity_token`](Self::sign_identity_token), but already
    /// expired — for the expiry-rejection test.
    pub fn sign_expired_identity_token(&self, sub: &str) -> String {
        self.sign(sub, None, None, None, -3600)
    }

    fn sign(
        &self,
        sub: &str,
        email: Option<&str>,
        nonce: Option<&str>,
        jti: Option<&str>,
        expires_in_secs: i64,
    ) -> String {
        let now = Utc::now().timestamp();
        let claims = TestClaims {
            iss: self.issuer.clone(),
            aud: self.audience.clone(),
            sub: sub.to_owned(),
            exp: now + expires_in_secs,
            iat: now,
            email: email.map(str::to_owned),
            nonce: nonce.map(str::to_owned),
            jti: jti.map(str::to_owned),
        };
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(TEST_KID.to_owned());
        encode(&header, &claims, &self.encoding_key).expect("sign test token")
    }
}

#[async_trait]
impl OidcProvider for TestOidcProvider {
    fn is_cancellation(&self, error: &str) -> bool {
        error == "access_denied" || (self.name() == "apple" && error == "user_cancelled_authorize")
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn authorize_url(&self, state: &str, nonce: &str, redirect_uri: &str) -> String {
        let mut url = url::Url::parse("https://fake-idp.test/authorize").unwrap();
        url.query_pairs_mut()
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("redirect_uri", redirect_uri);
        url.to_string()
    }

    async fn exchange_code(
        &self,
        code: &str,
        _redirect_uri: &str,
    ) -> Result<UpstreamTokens, OidcError> {
        let pending =
            self.codes.lock().unwrap().remove(code).ok_or_else(|| {
                OidcError::Malformed("unknown test authorization code".to_owned())
            })?;
        let id_token = self.sign_identity_token(
            &pending.sub,
            pending.email.as_deref(),
            Some(&pending.nonce),
            None,
        );
        Ok(UpstreamTokens { id_token })
    }

    async fn verify_identity_token(
        &self,
        token: &str,
        nonce: Option<&str>,
    ) -> Result<VerifiedIdentity, OidcError> {
        let mut validation = Validation::new(Algorithm::ES256);
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.validate_aud = false;
        let data = decode::<TestClaims>(token, &self.decoding_key, &validation)
            .map_err(|err| OidcError::BadSignature(err.to_string()))?;
        let claims = data.claims;

        if claims.iss != self.issuer {
            return Err(OidcError::InvalidIssuer(claims.iss));
        }
        if claims.aud != self.audience {
            return Err(OidcError::InvalidAudience(claims.aud));
        }
        if let Some(expected) = nonce {
            if claims.nonce.as_deref() != Some(expected) {
                return Err(OidcError::InvalidNonce);
            }
        }
        let now = Utc::now().timestamp();
        if claims.exp < now {
            return Err(OidcError::Expired(format!(
                "token expired at {}",
                claims.exp
            )));
        }

        let replay_key = claims
            .jti
            .clone()
            .unwrap_or_else(|| format!("hash:{token}"));
        Ok(VerifiedIdentity {
            subject: claims.sub,
            email: claims.email,
            replay_key,
        })
    }
}
