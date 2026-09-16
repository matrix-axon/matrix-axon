//! OAuth 2.0 authorization server (M14, ADR 0054).
//!
//! Axon is its own OAuth 2.0 Authorization Server to its clients (public
//! clients, PKCE mandatory) and an OIDC Relying Party to Apple/Google/
//! Microsoft, purely to answer "is this the bound owner?". See the ADR for
//! the full design; this module tree is the implementation:
//!
//! - [`provider`]: the [`OidcProvider`](provider::OidcProvider) port.
//! - [`jwks`]: cached, refresh-rate-limited JWKS lookup.
//! - [`generic`]: the discovery-doc-driven provider impl (Google, Microsoft).
//! - [`tokens`]: mint/verify/rotate orchestration atop `axon-store`.
//! - [`rate_limit`]: the per-IP/per-`state` token-bucket layer.

pub mod generic;
pub mod jwks;
pub mod provider;
pub mod rate_limit;
pub mod tokens;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axon_core::OauthConfig;

pub use generic::GenericOidcProvider;
pub use provider::{OidcError, OidcProvider, UpstreamTokens, VerifiedIdentity};

/// Per-request timeout for every outbound call this module makes (discovery,
/// JWKS, token-exchange) — an unauthenticated surface must never let an
/// upstream provider hold a connection open indefinitely.
pub const OUTBOUND_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard cap on any single outbound oauth HTTP response body (discovery, JWKS,
/// or token-exchange), so a compromised or misbehaving upstream endpoint
/// can't hand axon an unbounded body to buffer in memory.
pub(crate) const MAX_HTTP_RESPONSE_BYTES: usize = 1024 * 1024;

/// Read `response`'s body, rejecting it outright if it exceeds `max_bytes`,
/// then parse it as JSON. Every outbound oauth HTTP call reads its body
/// through this rather than `Response::json()` directly, which imposes no
/// size limit of its own. Reads chunk-by-chunk and bails the moment the cap
/// is crossed, so `max_bytes` bounds how much this ever buffers — a
/// misbehaving upstream can't force a full oversized body into memory before
/// the length check runs (`Response::bytes()` would do exactly that).
pub(crate) async fn read_json_capped<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<T, OidcError> {
    let mut buf = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| OidcError::Http(err.to_string()))?
    {
        buf.extend_from_slice(&chunk);
        if buf.len() > max_bytes {
            return Err(OidcError::Malformed(format!(
                "response body exceeded the {max_bytes} byte cap"
            )));
        }
    }
    serde_json::from_slice(&buf).map_err(|err| OidcError::Malformed(err.to_string()))
}

/// Everything the `/v1/oauth/*` handlers share: resolved config, the
/// constructed provider set, and the rate limiter guarding the whole
/// sub-router. Held as `Option<Arc<OAuthRuntime>>` in `AppState` — `None`
/// disables the surface entirely (`oauth.enabled = false`), same pattern as
/// `AppState::search`.
pub struct OAuthRuntime {
    /// This axon instance's externally-reachable base URL, e.g.
    /// `https://myaxon.example.com` (no trailing slash).
    pub external_base_url: String,
    /// How long a minted access token verifies for.
    pub access_token_ttl: Duration,
    /// How long a minted refresh token is redeemable for.
    pub refresh_token_ttl: Duration,
    /// Statically pre-registered clients, keyed by `client_id`.
    pub clients: HashMap<String, Vec<String>>,
    /// Constructed providers, keyed by provider name (`"google"`,
    /// `"microsoft"`, `"apple"`). Only providers with `enabled = true` in
    /// config are present.
    pub providers: HashMap<&'static str, Arc<dyn OidcProvider>>,
    /// Per-IP / per-`state` token-bucket rate limiter over `/v1/oauth/*`.
    pub rate_limiter: rate_limit::OAuthRateLimiter,
}

impl OAuthRuntime {
    /// Build the runtime from config and an already-constructed provider set
    /// (constructing providers is async — discovery fetch — so it happens in
    /// the binary's composition root, not here).
    pub fn new(
        config: &OauthConfig,
        providers: HashMap<&'static str, Arc<dyn OidcProvider>>,
    ) -> Self {
        let clients = config
            .clients
            .iter()
            .map(|c| (c.client_id.clone(), c.redirect_uris.clone()))
            .collect();
        Self {
            external_base_url: config
                .external_base_url
                .clone()
                .unwrap_or_default()
                .trim_end_matches('/')
                .to_owned(),
            access_token_ttl: Duration::from_secs(config.access_token_ttl_secs),
            refresh_token_ttl: Duration::from_secs(config.refresh_token_ttl_secs),
            clients,
            providers,
            rate_limiter: rate_limit::OAuthRateLimiter::new(),
        }
    }

    /// Look up a constructed provider by name, if it's enabled and wired up.
    pub fn provider(&self, name: &str) -> Option<&Arc<dyn OidcProvider>> {
        self.providers.get(name)
    }

    /// Whether `redirect_uri` is exactly (not prefix-) allow-listed for
    /// `client_id`, and if not, which half was wrong.
    ///
    /// Exact matching is the one place `client_id` is a genuine security
    /// boundary in this design (Path A) — a loose match would let a malicious
    /// app registered under the same `client_id` redirect an authorization
    /// code to itself.
    ///
    /// The two failures are reported apart rather than collapsed into one
    /// `bool`, which is what the caller needs to say anything useful. That
    /// costs only the knowledge that a client id exists, and these ids are
    /// public by construction: they are RFC 8252 public clients, registered in
    /// operator config, shipped inside the client binaries and printed in
    /// `clients/web/README.md`. There is nothing here to enumerate, and
    /// `/v1/oauth/*` is rate limited regardless (`oauth::rate_limit`).
    pub fn check_client(&self, client_id: &str, redirect_uri: &str) -> ClientCheck {
        let Some(uris) = self.clients.get(client_id) else {
            return ClientCheck::UnknownClient;
        };
        if uris.iter().any(|u| u == redirect_uri) {
            ClientCheck::Allowed
        } else {
            ClientCheck::RedirectUriNotRegistered
        }
    }
}

/// The verdict on a `client_id`/`redirect_uri` pair.
///
/// The two refusals are distinct because they need different things of
/// whoever reads them: one means the client was never registered, the other
/// that it was but with a different callback — the shape of a client and a
/// server that have drifted apart, which is by far the more common of the two
/// and the one a single combined message could not name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientCheck {
    /// Registered, with this exact redirect URI.
    Allowed,
    /// No client is registered under this id.
    UnknownClient,
    /// The client is registered, but not for this redirect URI.
    RedirectUriNotRegistered,
}

/// Build the shared HTTP client used for every outbound oauth call
/// (discovery, JWKS, token-exchange): a bounded per-request timeout,
/// otherwise stock — same precedent as `axon-sync`'s homeserver-discovery
/// client.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(OUTBOUND_HTTP_TIMEOUT)
        .build()
        .expect("oauth HTTP client must build")
}
