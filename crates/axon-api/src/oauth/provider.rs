//! The [`OidcProvider`] port: axon as an OIDC Relying Party (M14b, ADR 0054).
//!
//! Axon is an OIDC Relying Party to Apple/Google/Microsoft purely to answer
//! "is this the bound owner?" — upstream tokens are consumed internally and
//! never handed to a client. [`GenericOidcProvider`](crate::oauth::generic::GenericOidcProvider)
//! covers Google and Microsoft (discovery-doc driven); Apple's browser provider
//! is registered when configured; native challenges are pending.

use async_trait::async_trait;

/// A verified upstream identity: the result of successfully checking an
/// id_token's signature and claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedIdentity {
    /// The token's `sub` claim — the upstream provider's stable identifier
    /// for this account. Matched against `oauth_identities` to find (or, for
    /// the bind flow, create) the local binding.
    pub subject: String,
    /// The token's `email` claim, if present.
    pub email: Option<String>,
    /// This token's replay-defense key: its `jti` claim, or a hash of the raw
    /// token when the provider omits `jti`. Path B's handler consumes this
    /// via [`Store::consume_identity_token`](axon_store::Store::consume_identity_token)
    /// before trusting the identity.
    pub replay_key: String,
}

/// The tokens returned by exchanging an upstream authorization code (Path A).
#[derive(Debug, Clone)]
pub struct UpstreamTokens {
    /// The provider's id_token (a JWT), still unverified at this point —
    /// [`OidcProvider::verify_identity_token`] does that next.
    pub id_token: String,
}

/// What can go wrong talking to, or verifying a token from, an upstream OIDC
/// provider.
#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    /// A network/transport failure reaching the provider (discovery, JWKS,
    /// or token-exchange endpoint).
    #[error("upstream request failed: {0}")]
    Http(String),
    /// The provider's discovery document or token response was not the shape
    /// expected.
    #[error("malformed upstream response: {0}")]
    Malformed(String),
    /// The token's `iss` did not match this provider's configured issuer.
    #[error("unexpected issuer: {0}")]
    InvalidIssuer(String),
    /// The token's `aud` did not match any configured audience.
    #[error("unexpected audience: {0}")]
    InvalidAudience(String),
    /// The token's `nonce` did not match the one axon sent upstream (Path A),
    /// or a nonce was required (Path A) but absent.
    #[error("nonce mismatch")]
    InvalidNonce,
    /// The token is expired, not yet valid, or issued in the future beyond
    /// the allowed clock skew.
    #[error("token is not currently valid: {0}")]
    Expired(String),
    /// The token's signature did not verify, or the key it named (`kid`) was
    /// never found.
    #[error("bad signature: {0}")]
    BadSignature(String),
    /// The token named an algorithm outside this provider's allow-list
    /// (RS256/ES256 only — never `none` or an HMAC family).
    #[error("disallowed algorithm: {0}")]
    DisallowedAlgorithm(String),
}

impl OidcError {
    /// Safe diagnostic category; never format the attached upstream value.
    pub fn diagnostic_reason(&self) -> &'static str {
        match self {
            Self::Http(_) => "upstream_request_failed",
            Self::Malformed(_) => "upstream_response_invalid",
            Self::InvalidIssuer(_) => "issuer_mismatch",
            Self::InvalidAudience(_) => "audience_mismatch",
            Self::InvalidNonce => "nonce_mismatch",
            Self::Expired(_) => "token_time_invalid",
            Self::BadSignature(_) => "signature_invalid",
            Self::DisallowedAlgorithm(_) => "algorithm_disallowed",
        }
    }
}

/// The seam between Path A/B's HTTP handlers and however a given upstream
/// provider actually works. Held as `Arc<dyn OidcProvider>` in
/// [`OAuthRuntime`](crate::oauth::OAuthRuntime), one per enabled provider.
#[async_trait]
pub trait OidcProvider: Send + Sync {
    /// This provider's short name (`"apple"`, `"google"`, `"microsoft"`) —
    /// also the `provider` column value stored alongside every row that
    /// references it.
    fn name(&self) -> &'static str;

    /// Recognize cancellation without exposing the provider's raw error text.
    fn is_cancellation(&self, error: &str) -> bool {
        error == "access_denied"
    }

    /// Build the URL axon redirects the browser to, starting Path A's
    /// upstream leg. `state`/`nonce` are axon's own CSRF-binding values for
    /// this leg (distinct from the client's own `state`, which axon tracks
    /// separately — see `oauth_authorization_requests.client_state`).
    fn authorize_url(&self, state: &str, nonce: &str, redirect_uri: &str) -> String;

    /// Exchange an upstream authorization code (from the provider's callback)
    /// for its tokens. `redirect_uri` must be the same value passed to
    /// [`authorize_url`](Self::authorize_url) — most providers validate this.
    async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
    ) -> Result<UpstreamTokens, OidcError>;

    /// Verify a signed identity token (an id_token from either path) and
    /// return the identity it asserts. `nonce`, when `Some`, must match the
    /// token's `nonce` claim exactly (Path A); `None` means no nonce check is
    /// expected for the existing Google/Microsoft Path B. Apple rejects
    /// `None`; its native verifier requires a server-issued challenge nonce.
    async fn verify_identity_token(
        &self,
        token: &str,
        nonce: Option<&str>,
    ) -> Result<VerifiedIdentity, OidcError>;
}
