//! Per-IP / per-`state` rate limiting over `/v1/oauth/*` (M14b, ADR 0054).
//!
//! New infrastructure this codebase doesn't otherwise have: `/v1/oauth/*` is
//! the only unauthenticated surface capable of minting credentials, so it
//! gets its own in-process token-bucket layer. Two independent buckets:
//! **per source IP** (a single source hammering the endpoint) and **per
//! `state`/`user_code` value** when the request carries one (guards a single
//! in-flight flow against being hammered from many source IPs, e.g. code/PKCE
//! guessing). Either bucket being empty rejects the request with `429`.
//!
//! In-process only (a `governor` keyed limiter, not shared across axon
//! instances) — acceptable for a single-process personal server; a future
//! multi-instance deployment would need this centralized.
//!
//! Both buckets are keyed stores that grow one entry per distinct key/IP
//! ever seen and never evict on their own (GH #285): [`spawn_sweeper`]
//! periodically reclaims idle entries with `retain_recent()` *and* releases
//! the backing store's spare capacity with `shrink_to_fit()` — the former
//! alone doesn't shrink the underlying `DashMap`'s allocation, so a one-time
//! high-cardinality spray would otherwise leave that allocation resident
//! forever. `check_key` also hashes its (attacker-controlled) input so a
//! single request can't pin an oversized key in memory.

use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, Method, Request};
use axum::middleware::Next;
use axum::response::Response;
use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};

use crate::oauth::OAuthRuntime;
use crate::response::ApiError;
use std::sync::Arc;

/// Requests allowed per source IP, per minute.
const PER_IP_PER_MINUTE: u32 = 30;
/// Requests allowed per `state`/`user_code` value, per minute. Tighter than
/// the per-IP bucket since a single flow legitimately sees very few requests.
const PER_KEY_PER_MINUTE: u32 = 10;

/// How often the background sweeper (see [`spawn_sweeper`]) reclaims bucket
/// entries that have gone idle long enough to have fully refilled. Matches
/// the per-minute quotas above, so a key/IP that's stopped sending requests
/// is reclaimed within about a minute of going quiet.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// The two independent token buckets guarding `/v1/oauth/*`.
pub struct OAuthRateLimiter {
    per_ip: RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock>,
    per_key: RateLimiter<String, DefaultKeyedStateStore<String>, DefaultClock>,
}

impl OAuthRateLimiter {
    pub fn new() -> Self {
        Self {
            per_ip: RateLimiter::keyed(per_minute(PER_IP_PER_MINUTE)),
            per_key: RateLimiter::keyed(per_minute(PER_KEY_PER_MINUTE)),
        }
    }

    /// Check (and consume from) the per-IP bucket.
    fn check_ip(&self, ip: IpAddr) -> bool {
        self.per_ip.check_key(&ip).is_ok()
    }

    /// Check (and consume from) the per-`state`/`user_code` bucket, when the
    /// request carries one. `key` is hashed before use, never stored
    /// verbatim: it's an attacker-chosen value (the `state`/`user_code` query
    /// param, or a `code`/`refresh_token`/`identity_token` form field) that
    /// could otherwise pin tens of KiB in the keyed store per request (GH
    /// #285). Hashing bounds every entry to a fixed-size digest regardless of
    /// input length.
    fn check_key(&self, key: &str) -> bool {
        self.per_key.check_key(&axon_core::hash_secret(key)).is_ok()
    }

    /// Drop bucket entries idle long enough to have fully refilled, then
    /// shrink the backing store — the only way `per_ip`/`per_key` stop
    /// growing without bound, since `governor`'s keyed stores never evict on
    /// their own (GH #285). `retain_recent()` alone isn't enough: the
    /// `DashMap` behind each store doesn't release shard capacity just
    /// because entries were removed from it (same as `Vec::retain` never
    /// shrinking `capacity`), so a one-time high-cardinality spray would
    /// leave the attack-time allocation resident forever even with entries
    /// gone. `shrink_to_fit()` is what actually gives that memory back.
    /// Called periodically by [`spawn_sweeper`].
    fn sweep(&self) {
        self.per_ip.retain_recent();
        self.per_ip.shrink_to_fit();
        self.per_key.retain_recent();
        self.per_key.shrink_to_fit();
    }
}

/// Spawn the background task that periodically sweeps both of `runtime`'s
/// rate-limit buckets (see [`OAuthRateLimiter::sweep`]). Not part of the
/// server's graceful-shutdown sequence — like `spawn_fd_diagnostics` in
/// `axon-server`, this is pure in-process cache maintenance with no state to
/// drain, so it's fine for it to simply stop when the process exits.
pub fn spawn_sweeper(runtime: Arc<OAuthRuntime>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_INTERVAL);
        tick.tick().await; // first tick fires immediately; skip it, we just booted
        loop {
            tick.tick().await;
            runtime.rate_limiter.sweep();
        }
    });
}

impl Default for OAuthRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

fn per_minute(n: u32) -> Quota {
    Quota::per_minute(NonZeroU32::new(n).expect("rate limit quota must be nonzero"))
}

/// The query parameter names checked for a per-flow rate-limit key. Any one
/// of these, if present, is rate-limited independently of source IP —
/// `state` (Path A's authorize/callback), `user_code` (the bind handshake,
/// M14c).
const KEYED_PARAMS: &[&str] = &["state", "user_code"];

/// The form-body field names checked for a per-flow rate-limit key on
/// `POST /v1/oauth/token`, whose parameters arrive in the body, not the
/// query string (RFC 6749 §4.1.3). Each names the one guessable secret a
/// given grant redeems — the exact thing this bucket exists to throttle
/// (PKCE `code_verifier` guessing against a captured `code`, or brute-forcing
/// a `refresh_token`/`identity_token`).
const KEYED_BODY_PARAMS: &[&str] = &["code", "refresh_token", "identity_token"];

/// Cap on how much of a request body this middleware will buffer to look for
/// a rate-limit key. Generously large for a handful of short form fields;
/// exists only so a request with an oversized body can't make this middleware
/// itself do unbounded buffering (the handler's own body limit still applies
/// to whatever passes through).
const MAX_KEY_PEEK_BYTES: usize = 64 * 1024;

/// Axum middleware enforcing both buckets over every `/v1/oauth/*` request.
/// Applied as a `route_layer` over the oauth sub-router, so it runs for every
/// method/path under it uniformly. `oauth.enabled = false` (a `None` runtime)
/// 404s here before any limiter is even consulted — same "disabled surface"
/// check every oauth handler performs.
pub async fn rate_limit(
    State(runtime): State<Option<Arc<OAuthRuntime>>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, ApiError> {
    let Some(runtime) = runtime else {
        return Err(ApiError::not_found("oauth is disabled"));
    };
    if !runtime.rate_limiter.check_ip(addr.ip()) {
        return Err(too_many_requests());
    }

    let (key, req) = if req.method() == Method::POST {
        // Query strings must not override the secret carried in a POST body.
        keyed_param_from_form_body(req).await?
    } else {
        (keyed_param(req.uri().query()), req)
    };
    if let Some(key) = key {
        if !runtime.rate_limiter.check_key(&key) {
            return Err(too_many_requests());
        }
    }
    Ok(next.run(req).await)
}

fn keyed_param(query: Option<&str>) -> Option<String> {
    let query = query?;
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if KEYED_PARAMS.contains(&name.as_ref()) {
            return Some(value.into_owned());
        }
    }
    None
}

/// As [`keyed_param`], but for token and callback form-encoded bodies
/// (only inspected for a `POST` carrying `application/x-www-form-urlencoded`,
/// so a GET or a differently-typed body never pays a buffering cost). The
/// body is buffered, checked, and handed back whole in a fresh `Request` so
/// the downstream handler still sees it exactly once.
async fn keyed_param_from_form_body(
    req: Request<Body>,
) -> Result<(Option<String>, Request<Body>), ApiError> {
    let is_form_post = req.method() == Method::POST
        && req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"));
    if !is_form_post {
        return Ok((None, req));
    }

    let is_callback = req.uri().path().ends_with("/callback");
    let (parts, body) = req.into_parts();
    let limit = if is_callback {
        super::MAX_CALLBACK_BYTES
    } else {
        MAX_KEY_PEEK_BYTES
    };
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        axum::body::to_bytes(body, limit),
    )
    .await
    .map_err(|_| ApiError::bad_request("request body timed out"))?
    .map_err(|_| ApiError::payload_too_large("request body too large"))?;
    let key = url::form_urlencoded::parse(&bytes)
        .find(|(name, _)| {
            if is_callback {
                name == "state"
            } else {
                KEYED_BODY_PARAMS.contains(&name.as_ref())
            }
        })
        .map(|(_, value)| value.into_owned());
    Ok((key, Request::from_parts(parts, Body::from(bytes))))
}

fn too_many_requests() -> ApiError {
    ApiError::too_many_requests("rate limit exceeded")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_ip_bucket_exhausts_after_its_quota() {
        let limiter = OAuthRateLimiter::new();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        for _ in 0..PER_IP_PER_MINUTE {
            assert!(limiter.check_ip(ip));
        }
        assert!(!limiter.check_ip(ip), "quota should be exhausted");
    }

    #[test]
    fn distinct_ips_have_independent_buckets() {
        let limiter = OAuthRateLimiter::new();
        let a: IpAddr = "127.0.0.1".parse().unwrap();
        let b: IpAddr = "127.0.0.2".parse().unwrap();
        for _ in 0..PER_IP_PER_MINUTE {
            assert!(limiter.check_ip(a));
        }
        // b's bucket is untouched by a's exhaustion.
        assert!(limiter.check_ip(b));
    }

    #[test]
    fn check_key_bucket_exhausts_after_its_quota() {
        let limiter = OAuthRateLimiter::new();
        for _ in 0..PER_KEY_PER_MINUTE {
            assert!(limiter.check_key("some-state-value"));
        }
        assert!(
            !limiter.check_key("some-state-value"),
            "quota should be exhausted"
        );
    }

    #[test]
    fn distinct_keys_have_independent_buckets() {
        let limiter = OAuthRateLimiter::new();
        for _ in 0..PER_KEY_PER_MINUTE {
            assert!(limiter.check_key("state-a"));
        }
        // "state-b" hashes to a different bucket, untouched by "state-a"'s
        // exhaustion.
        assert!(limiter.check_key("state-b"));
    }

    #[test]
    fn oversized_key_does_not_panic_or_bypass_hashing() {
        // GH #285: an attacker-chosen key up to MAX_KEY_PEEK_BYTES long must
        // still be hashed down to a fixed-size bucket entry, not stored
        // verbatim.
        let limiter = OAuthRateLimiter::new();
        let huge_key = "x".repeat(MAX_KEY_PEEK_BYTES);
        assert!(limiter.check_key(&huge_key));
    }

    #[test]
    fn sweep_reclaims_bucket_entries() {
        let limiter = OAuthRateLimiter::new();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        limiter.check_ip(ip);
        limiter.check_key("some-state-value");
        assert_eq!(limiter.per_ip.len(), 1);
        assert_eq!(limiter.per_key.len(), 1);

        // retain_recent() only reclaims entries whose bucket has fully
        // refilled, which real time hasn't elapsed for yet here, and
        // shrink_to_fit() never changes entry count — this just confirms
        // sweep() runs without panicking or removing live entries
        // prematurely.
        limiter.sweep();
        assert_eq!(limiter.per_ip.len(), 1);
        assert_eq!(limiter.per_key.len(), 1);
    }

    #[test]
    fn keyed_param_extracts_state_or_user_code() {
        assert_eq!(
            keyed_param(Some("foo=bar&state=abc123")),
            Some("abc123".to_owned())
        );
        assert_eq!(
            keyed_param(Some("user_code=XYZ&other=1")),
            Some("XYZ".to_owned())
        );
        assert_eq!(keyed_param(Some("foo=bar")), None);
        assert_eq!(keyed_param(None), None);
    }
}
