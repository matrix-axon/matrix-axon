//! Bearer-token authentication for the `/v1/` API (M7b).
//!
//! Every `/v1/…` route — HTTP and the WebSocket — requires a valid bearer token.
//! [`require_bearer`] is the HTTP middleware (applied as a layer over the `/v1`
//! routes in [`router`](crate::router)); the WebSocket handler does its own check
//! at upgrade time (a browser can't set an `Authorization` header on a socket —
//! see [`ws`](crate::ws)). Both go through a [`TokenVerifier`].
//!
//! `TokenVerifier` is the seam the spec calls for: the CLI mint path and the
//! `tokens` table are an implementation detail behind it, so a future OAuth 2.0 +
//! PKCE issuer can replace them without changing the on-the-wire
//! `Authorization: Bearer` contract or any consumer code. The shipped
//! implementation is [`StoreTokenVerifier`], backed by [`Store`].

use std::sync::Arc;

use async_trait::async_trait;
use axon_store::Store;
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::response::ApiError;

/// The bare RFC 6750 §3 challenge, advertised on a `401` when no usable bearer
/// credential was presented (missing or malformed `Authorization` header).
const CHALLENGE_BEARER: HeaderValue = HeaderValue::from_static("Bearer");
/// The RFC 6750 §3.1 challenge for a syntactically present but rejected token
/// (unknown or revoked), advertised on a `401` so a standards-aware client knows
/// the token — not the scheme — was the problem.
const CHALLENGE_INVALID_TOKEN: HeaderValue =
    HeaderValue::from_static("Bearer error=\"invalid_token\"");

/// Validates a presented bearer token. The seam between the API's auth gate and
/// however tokens are actually issued/validated — held in
/// [`AppState`](crate::AppState) as `Arc<dyn TokenVerifier>`.
#[async_trait]
pub trait TokenVerifier: Send + Sync {
    /// Whether `token` (the raw bearer string, sans the `Bearer ` prefix) is
    /// currently valid. `Ok(false)` is an unknown or revoked token; `Err` is an
    /// infrastructure failure (e.g. the store), surfaced to the client as `500`.
    async fn verify(&self, token: &str) -> Result<bool, ApiError>;
}

/// The shipped [`TokenVerifier`]: hashes the presented token and looks it up in
/// the `tokens` table via [`Store::verify_token`] (which also stamps
/// `last_used_at`).
#[derive(Clone)]
pub struct StoreTokenVerifier {
    store: Store,
}

impl StoreTokenVerifier {
    /// Build a verifier over the given [`Store`] handle.
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

#[async_trait]
impl TokenVerifier for StoreTokenVerifier {
    async fn verify(&self, token: &str) -> Result<bool, ApiError> {
        // A store failure converts into a logged 500 via `From<StoreError>`.
        Ok(self.store.verify_token(token).await?.is_some())
    }
}

/// Extract the raw token from an `Authorization: Bearer <token>` header, if
/// present and well-formed. Shared by the HTTP middleware and the WebSocket
/// upgrade so the two parse the header identically.
///
/// The auth *scheme* is matched case-insensitively (`Bearer`, `bearer`, `BEARER`
/// are all valid per RFC 7235 §2.1); the token itself is taken verbatim.
pub(crate) fn bearer_from_headers(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// A `401` for a *missing or malformed* bearer credential, carrying the bare
/// `WWW-Authenticate: Bearer` challenge (RFC 6750 §3).
///
/// These challenge helpers live on the gate — not on
/// [`ApiError::unauthorized`](crate::response::ApiError::unauthorized) — on
/// purpose: a `401` raised *inside* a handler (e.g. `login`, when the **Matrix**
/// homeserver rejects the supplied Matrix credentials) is a different failure and
/// must not advertise a client↔axon bearer challenge.
pub(crate) fn missing_token_response(message: impl Into<String>) -> Response {
    challenge(message, CHALLENGE_BEARER)
}

/// A `401` for a *present but rejected* token (unknown or revoked), carrying
/// `WWW-Authenticate: Bearer error="invalid_token"` (RFC 6750 §3.1). See
/// [`missing_token_response`] for why this is not on `ApiError`.
pub(crate) fn invalid_token_response() -> Response {
    tracing::warn!(
        reason = "invalid_bearer_token",
        "Axon authentication rejected"
    );
    challenge(
        "The access token is invalid, expired, or revoked. Sign in again or ask the instance owner for a new token.",
        CHALLENGE_INVALID_TOKEN,
    )
}

/// Build the enveloped `401` and attach the given RFC 6750 `WWW-Authenticate`
/// challenge.
fn challenge(message: impl Into<String>, challenge: HeaderValue) -> Response {
    let mut response = ApiError::unauthorized(message).into_response();
    response.headers_mut().insert(WWW_AUTHENTICATE, challenge);
    response
}

/// HTTP middleware enforcing a valid bearer token on every request it guards.
/// A missing or malformed `Authorization` header, or a token that fails
/// verification, is a `401` carrying the appropriate `WWW-Authenticate: Bearer`
/// challenge (RFC 6750); the guarded handler runs only on success.
///
/// The [`TokenVerifier`] is pulled from router state via `State`, so the same
/// guard works for any verifier implementation.
pub async fn require_bearer(
    State(verifier): State<Arc<dyn TokenVerifier>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(token) = bearer_from_headers(req.headers()) else {
        return missing_token_response("missing or malformed bearer token");
    };

    match verifier.verify(token).await {
        Ok(true) => next.run(req).await,
        Ok(false) => invalid_token_response(),
        Err(err) => err.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn header(value: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static(value));
        headers
    }

    #[test]
    fn parses_a_well_formed_bearer_header() {
        assert_eq!(
            bearer_from_headers(&header("Bearer axon_abc")),
            Some("axon_abc")
        );
    }

    #[test]
    fn scheme_match_is_case_insensitive() {
        // The scheme is case-insensitive (RFC 7235); the token is not.
        assert_eq!(
            bearer_from_headers(&header("bearer axon_abc")),
            Some("axon_abc")
        );
        assert_eq!(
            bearer_from_headers(&header("BEARER axon_abc")),
            Some("axon_abc")
        );
        assert_eq!(
            bearer_from_headers(&header("BeArEr axon_abc")),
            Some("axon_abc")
        );
    }

    #[test]
    fn rejects_missing_or_malformed_headers() {
        assert_eq!(bearer_from_headers(&HeaderMap::new()), None);
        assert_eq!(bearer_from_headers(&header("Basic abc")), None);
        assert_eq!(bearer_from_headers(&header("Bearer ")), None);
        // A scheme with no separating space is malformed.
        assert_eq!(bearer_from_headers(&header("Bearer")), None);
    }
}
