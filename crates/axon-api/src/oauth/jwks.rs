//! Cached, refresh-rate-limited JWKS lookup (M14b, ADR 0054).
//!
//! The single riskiest piece of new logic in M14b: an attacker who can send
//! id_tokens with an arbitrary `kid` could otherwise force a fresh JWKS fetch
//! on every request (fetch-amplification against the upstream provider, or a
//! denial-of-service against axon's own outbound connection pool). The
//! defense is a **minimum refresh interval per provider, applied even on a
//! cache miss**: an unknown `kid` triggers at most one refetch per interval,
//! never one per request.

use std::time::{Duration, Instant};

use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, Jwk, JwkSet};
use jsonwebtoken::{Algorithm, DecodingKey};
use tokio::sync::Mutex;

use crate::oauth::provider::OidcError;

/// Floor on how often a cache miss (unknown `kid`) is allowed to trigger a
/// real fetch against the provider's JWKS endpoint.
const DEFAULT_MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Hard cap on a JWKS response body, so a compromised or misbehaving
/// `jwks_uri` can't hand axon an unbounded body to buffer in memory.
const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// A cached view of one provider's JWKS document, refetched at most once per
/// [`DEFAULT_MIN_REFRESH_INTERVAL`] even when every lookup misses.
pub struct JwksCache {
    http: reqwest::Client,
    jwks_uri: String,
    min_refresh_interval: Duration,
    max_response_bytes: usize,
    state: Mutex<CacheState>,
}

#[derive(Default)]
struct CacheState {
    jwks: Option<JwkSet>,
    last_fetched: Option<Instant>,
}

impl JwksCache {
    /// Build a cache over the given JWKS endpoint. `http` should already
    /// carry a bounded per-request timeout (see [`OAuthRuntime`](crate::oauth::OAuthRuntime)).
    pub fn new(http: reqwest::Client, jwks_uri: String) -> Self {
        Self {
            http,
            jwks_uri,
            min_refresh_interval: DEFAULT_MIN_REFRESH_INTERVAL,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            state: Mutex::new(CacheState::default()),
        }
    }

    #[cfg(test)]
    fn with_min_refresh_interval(mut self, interval: Duration) -> Self {
        self.min_refresh_interval = interval;
        self
    }

    /// Resolve `kid` to a decoding key and the algorithm it must be used
    /// with. Serves from cache when possible; on a miss, fetches fresh JWKS
    /// **only if** the minimum refresh interval has elapsed since the last
    /// fetch — otherwise fails closed rather than hitting the network again.
    ///
    /// The cache lock is never held across the network fetch: a cache hit
    /// (the common case) only ever needs the lock for the instant it takes
    /// to read the map, so concurrent lookups for other, already-cached
    /// `kid`s never queue behind one caller's in-flight fetch. On a miss,
    /// the refresh slot (`last_fetched`) is claimed under the lock *before*
    /// releasing it and starting the fetch, so two concurrent misses can't
    /// both win the "am I allowed to refetch" check and double-fetch.
    pub async fn resolve(&self, kid: &str) -> Result<(Algorithm, DecodingKey), OidcError> {
        if let Some(found) = self.cached(kid).await {
            return found;
        }

        let now = Instant::now();
        {
            let mut state = self.state.lock().await;
            // Re-check under the lock: another task may have populated this
            // exact kid while we were re-acquiring after the first check.
            if let Some(found) = state.jwks.as_ref().and_then(|set| find_key(set, kid)) {
                return found;
            }
            let refresh_allowed = state
                .last_fetched
                .is_none_or(|last| now.duration_since(last) >= self.min_refresh_interval);
            if !refresh_allowed {
                return Err(OidcError::BadSignature(format!(
                    "unknown key id {kid:?} and a JWKS refresh already happened within the last {:?}",
                    self.min_refresh_interval
                )));
            }
            // Claim the refresh slot now, before releasing the lock and
            // making the network call, so a concurrent miss can't also pass
            // the check above and trigger a second fetch.
            state.last_fetched = Some(now);
        }

        let fetched = self.fetch().await?;
        let result = find_key(&fetched, kid);
        self.state.lock().await.jwks = Some(fetched);
        result.unwrap_or_else(|| Err(OidcError::BadSignature(format!("unknown key id {kid:?}"))))
    }

    /// A quick, lock-scoped cache lookup with no network involvement.
    async fn cached(&self, kid: &str) -> Option<Result<(Algorithm, DecodingKey), OidcError>> {
        let state = self.state.lock().await;
        state.jwks.as_ref().and_then(|set| find_key(set, kid))
    }

    async fn fetch(&self) -> Result<JwkSet, OidcError> {
        let response = self
            .http
            .get(&self.jwks_uri)
            .send()
            .await
            .map_err(|err| super::transport_error("JWKS request", err))?;
        if !response.status().is_success() {
            return Err(OidcError::Http(format!(
                "JWKS request returned {}",
                response.status()
            )));
        }
        crate::oauth::read_json_capped(response, self.max_response_bytes).await
    }
}

/// Find `kid` in `set` and build its `(Algorithm, DecodingKey)`, enforcing
/// the RS256/ES256 allow-list — any other key type (or an EC curve other
/// than P-256) is rejected rather than silently accepted.
fn find_key(set: &JwkSet, kid: &str) -> Option<Result<(Algorithm, DecodingKey), OidcError>> {
    let jwk = set.find(kid)?;
    Some(algorithm_and_key(jwk))
}

fn algorithm_and_key(jwk: &Jwk) -> Result<(Algorithm, DecodingKey), OidcError> {
    let algorithm = match &jwk.algorithm {
        AlgorithmParameters::RSA(_) => Algorithm::RS256,
        AlgorithmParameters::EllipticCurve(ec) if ec.curve == EllipticCurve::P256 => {
            Algorithm::ES256
        }
        other => {
            return Err(OidcError::DisallowedAlgorithm(format!(
                "JWKS key {:?} uses an unsupported key type: {other:?}",
                jwk.common.key_id
            )))
        }
    };
    let key = DecodingKey::from_jwk(jwk).map_err(|err| OidcError::Malformed(err.to_string()))?;
    Ok((algorithm, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    async fn serve(router: Router) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        addr
    }

    /// A minimal RSA JWK document with one key, `kid = "k1"`. The `n`/`e`
    /// values are RFC 7517 Appendix A.1's published example key — a public
    /// test vector, not a real provider's key — used here only to exercise
    /// cache/refresh behavior, not signature verification.
    fn rsa_jwks_body() -> serde_json::Value {
        json!({
            "keys": [{
                "kty": "RSA",
                "kid": "k1",
                "use": "sig",
                "alg": "RS256",
                "n": "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw",
                "e": "AQAB"
            }]
        })
    }

    #[tokio::test]
    async fn resolves_a_known_kid() {
        let addr =
            serve(Router::new().route("/jwks", get(|| async { Json(rsa_jwks_body()) }))).await;
        let cache = JwksCache::new(reqwest::Client::new(), format!("http://{addr}/jwks"));
        let (algorithm, _key) = cache.resolve("k1").await.expect("known kid resolves");
        assert_eq!(algorithm, Algorithm::RS256);
    }

    #[tokio::test]
    async fn unknown_kid_is_rejected_without_looping_forever() {
        let addr =
            serve(Router::new().route("/jwks", get(|| async { Json(rsa_jwks_body()) }))).await;
        let cache = JwksCache::new(reqwest::Client::new(), format!("http://{addr}/jwks"));
        let err = match cache.resolve("nope").await {
            Err(err) => err,
            Ok(_) => panic!("unknown kid must not resolve"),
        };
        assert!(matches!(err, OidcError::BadSignature(_)));
    }

    #[tokio::test]
    async fn a_second_miss_within_the_refresh_window_does_not_refetch() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        let addr = serve(Router::new().route(
            "/jwks",
            get(move || {
                let hits = counted.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    Json(rsa_jwks_body())
                }
            }),
        ))
        .await;
        let cache = JwksCache::new(reqwest::Client::new(), format!("http://{addr}/jwks"))
            .with_min_refresh_interval(Duration::from_secs(3600));

        // First lookup (of any kid) always fetches once.
        let _ = cache.resolve("nope").await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // A second unknown kid, still inside the refresh window, must not
        // trigger a second fetch — the amplification defense this cache exists
        // to provide.
        let _ = cache.resolve("still-nope").await;
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "a cache miss within the refresh window must not refetch"
        );
    }

    #[tokio::test]
    async fn oversized_response_is_rejected() {
        let addr = serve(Router::new().route(
            "/jwks",
            get(|| async { "x".repeat(DEFAULT_MAX_RESPONSE_BYTES + 1) }),
        ))
        .await;
        let cache = JwksCache::new(reqwest::Client::new(), format!("http://{addr}/jwks"));
        let err = match cache.resolve("k1").await {
            Err(err) => err,
            Ok(_) => panic!("oversized response must not resolve"),
        };
        assert!(matches!(err, OidcError::Malformed(_)));
    }
}
