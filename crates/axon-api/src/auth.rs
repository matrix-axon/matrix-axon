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

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axon_store::{Store, VerifiedToken};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::response::ApiError;

/// The bare RFC 6750 §3 challenge, advertised on a `401` when no usable bearer
/// credential was presented (missing or malformed `Authorization` header).
const CHALLENGE_BEARER: HeaderValue = HeaderValue::from_static("Bearer");
/// The RFC 6750 §3.1 challenge for a syntactically present but rejected token
/// (unknown or revoked), advertised on a `401` so a standards-aware client knows
/// the token — not the scheme — was the problem.
const CHALLENGE_INVALID_TOKEN: HeaderValue =
    HeaderValue::from_static("Bearer error=\"invalid_token\"");

/// How recent an OAuth session's interactive sign-in must be for it to change
/// credentials (ADR 0109).
pub const RECENT_SIGN_IN_WINDOW: chrono::Duration = chrono::Duration::minutes(10);

/// The bearer token that authenticated a request: what it is, never its
/// secret. [`require_bearer`] attaches it to the request as an extension, so a
/// handler behind the gate can take it as an [`axum::Extension`] or, for a
/// credential change, as [`CredentialChange`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthedToken {
    /// The token's id.
    pub id: Uuid,
    /// When it stops verifying; `None` for a token that never expires.
    pub expires_at: Option<DateTime<Utc>>,
    /// When the upstream sign-in behind its session completed, if it has one.
    pub authenticated_at: Option<DateTime<Utc>>,
    /// The bound identity it was minted for, if any.
    pub oauth_identity_id: Option<Uuid>,
}

impl AuthedToken {
    /// Whether this token may change credentials (ADR 0109): it never expires,
    /// or its session's interactive sign-in was within
    /// [`RECENT_SIGN_IN_WINDOW`] of `now`.
    ///
    /// A non-expiring token is what an operator mints on purpose and can
    /// revoke. An expiring one is an OAuth session, and a stolen access or
    /// refresh token carries the original sign-in time: only a fresh proof
    /// from the upstream provider moves it forward.
    pub fn may_change_credentials(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_none()
            || self
                .authenticated_at
                .is_some_and(|at| now.signed_duration_since(at) <= RECENT_SIGN_IN_WINDOW)
    }
}

impl From<VerifiedToken> for AuthedToken {
    fn from(token: VerifiedToken) -> Self {
        Self {
            id: token.id,
            expires_at: token.expires_at,
            authenticated_at: token.authenticated_at,
            oauth_identity_id: token.oauth_identity_id,
        }
    }
}

/// Proof that the request may change credentials: extracting it *is* the
/// step-up check, so a handler that mints, revokes, binds or unbinds cannot be
/// written without it. Rejects with `403 recent_sign_in_required`.
#[derive(Debug, Clone, Copy)]
pub struct CredentialChange(pub AuthedToken);

impl<S: Send + Sync> FromRequestParts<S> for CredentialChange {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // Absent only if a route was mounted outside `require_bearer`: a
        // wiring bug, and never a reason to let the request through.
        let token = parts
            .extensions
            .get::<AuthedToken>()
            .copied()
            .ok_or_else(|| {
                tracing::error!("credential-change route reached without an authenticated token");
                ApiError::internal()
            })?;
        if !token.may_change_credentials(Utc::now()) {
            tracing::info!(
                token_id = %token.id,
                reason = "recent_sign_in_required",
                "Credential change refused"
            );
            return Err(ApiError::recent_sign_in_required());
        }
        Ok(Self(token))
    }
}

/// Validates a presented bearer token. The seam between the API's auth gate and
/// however tokens are actually issued/validated — held in
/// [`AppState`](crate::AppState) as `Arc<dyn TokenVerifier>`.
#[async_trait]
pub trait TokenVerifier: Send + Sync {
    /// What `token` (the raw bearer string, sans the `Bearer ` prefix) is, if
    /// it is currently valid. `Ok(None)` is an unknown, revoked or expired
    /// token; `Err` is an infrastructure failure (e.g. the store), surfaced to
    /// the client as `500`.
    async fn verify(&self, token: &str) -> Result<Option<AuthedToken>, ApiError>;
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
    async fn verify(&self, token: &str) -> Result<Option<AuthedToken>, ApiError> {
        // A store failure converts into a logged 500 via `From<StoreError>`.
        Ok(self.store.verify_token(token).await?.map(AuthedToken::from))
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
    static WARNINGS: RejectionWarnings = RejectionWarnings(Mutex::new(None));
    if WARNINGS.allow(Instant::now()) {
        tracing::warn!(
            reason = "invalid_bearer_token",
            "Axon authentication rejected; further bearer warnings suppressed for 30 seconds"
        );
    } else {
        tracing::debug!(
            reason = "invalid_bearer_token",
            "Axon authentication rejected"
        );
    }
    challenge(
        "The access token is invalid, expired, or revoked. Sign in again or ask the instance owner for a new token.",
        CHALLENGE_INVALID_TOKEN,
    )
}

/// One process-wide slot, not an attacker-keyed map. HTTP and WS share it.
struct RejectionWarnings(Mutex<Option<Instant>>);

impl RejectionWarnings {
    fn allow(&self, now: Instant) -> bool {
        // Only timestamp comparison/update is protected: no I/O or await.
        // Contention must not masquerade as an active suppression window.
        let mut last = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if last.is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(30)) {
            return false;
        }
        *last = Some(now);
        true
    }
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
/// challenge (RFC 6750); the guarded handler runs only on success, with the
/// verified [`AuthedToken`] attached to the request as an extension.
///
/// The [`TokenVerifier`] is pulled from router state via `State`, so the same
/// guard works for any verifier implementation.
pub async fn require_bearer(
    State(verifier): State<Arc<dyn TokenVerifier>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(token) = bearer_from_headers(req.headers()) else {
        return missing_token_response("missing or malformed bearer token");
    };

    match verifier.verify(token).await {
        Ok(Some(authed)) => {
            req.extensions_mut().insert(authed);
            next.run(req).await
        }
        Ok(None) => invalid_token_response(),
        Err(err) => err.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn rejection_warning_waits_for_contended_slot() {
        let warnings = RejectionWarnings(Mutex::new(None));
        let guard = warnings.0.lock().unwrap();
        std::thread::scope(|scope| {
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let warnings = &warnings;
            let worker = scope.spawn(move || {
                result_tx.send(warnings.allow(Instant::now())).unwrap();
            });
            assert_eq!(
                result_rx.recv_timeout(Duration::from_millis(50)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            );
            drop(guard);
            assert!(result_rx.recv_timeout(Duration::from_secs(5)).unwrap());
            worker.join().unwrap();
        });
    }

    #[test]
    fn rejection_warnings_are_bounded_across_threads_and_time() {
        let warnings = Arc::new(RejectionWarnings(Mutex::new(None)));
        let now = Instant::now();
        let threads: Vec<_> = (0..32)
            .map(|_| {
                let warnings = warnings.clone();
                std::thread::spawn(move || warnings.allow(now))
            })
            .collect();
        assert_eq!(
            threads
                .into_iter()
                .filter_map(|thread| thread.join().ok())
                .filter(|allowed| *allowed)
                .count(),
            1
        );
        assert!(!warnings.allow(now + Duration::from_secs(29)));
        assert!(warnings.allow(now + Duration::from_secs(30)));
    }

    fn token(
        expires_in: Option<chrono::Duration>,
        signed_in_ago: Option<chrono::Duration>,
        now: DateTime<Utc>,
    ) -> AuthedToken {
        AuthedToken {
            id: Uuid::nil(),
            expires_at: expires_in.map(|d| now + d),
            authenticated_at: signed_in_ago.map(|d| now - d),
            oauth_identity_id: None,
        }
    }

    #[test]
    fn a_non_expiring_token_may_change_credentials_without_a_sign_in_time() {
        let now = Utc::now();
        assert!(token(None, None, now).may_change_credentials(now));
    }

    #[test]
    fn an_expiring_token_needs_a_sign_in_inside_the_window() {
        let now = Utc::now();
        let hour = Some(chrono::Duration::hours(1));
        let at = |ago| token(hour, Some(ago), now).may_change_credentials(now);
        assert!(at(chrono::Duration::zero()));
        assert!(at(RECENT_SIGN_IN_WINDOW));
        assert!(!at(RECENT_SIGN_IN_WINDOW + chrono::Duration::seconds(1)));
        assert!(!at(chrono::Duration::days(3)));
    }

    #[test]
    fn an_expiring_token_with_no_recorded_sign_in_is_not_recent() {
        // A session that began before the column existed, or a refresh chain
        // that never carried a time: unknown is not recent.
        let now = Utc::now();
        assert!(!token(Some(chrono::Duration::hours(1)), None, now).may_change_credentials(now));
    }

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
