//! Token orchestration atop `axon-store` (M14b, ADR 0054).
//!
//! Where Path A/B's HTTP handlers actually mint, verify, and rotate
//! credentials. Kept free of any axum types so it's testable without a
//! router — the handlers in [`routes::oauth`](crate::routes::oauth) are thin
//! wrappers translating these `Result`s into the plain RFC 6749 JSON body
//! `POST /v1/oauth/token` uses (not the `ApiResponse`/`ApiError` envelope the
//! rest of the API shares).

use base64::Engine;
use chrono::Utc;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use axon_store::{RedeemRefreshTokenError, Store, StoreError};

use crate::oauth::provider::OidcError;
use crate::oauth::OAuthRuntime;

/// A minted access + refresh token pair, ready to serialize as
/// `POST /v1/oauth/token`'s success body.
#[derive(Debug, Clone)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

/// Why a `POST /v1/oauth/token` request was rejected. Maps onto RFC 6749 §5.2
/// error codes in the HTTP layer, not `ApiError`.
#[derive(Debug)]
pub enum TokenError {
    /// The presented code/refresh-token/identity-token is unknown, expired,
    /// already used, or fails PKCE/redirect_uri/client_id matching — RFC 6749
    /// folds all of these into a single `invalid_grant` so as not to help an
    /// attacker distinguish "close" guesses from "nowhere close" ones.
    InvalidGrant(&'static str),
    /// The requested provider isn't configured/enabled.
    UnknownProvider,
    /// The verified upstream identity has no bound `oauth_identities` row —
    /// Path A/B only ever authenticate an *already-bound* owner; binding
    /// itself is the separate `axon oauth bind` flow (M14c).
    NotBound,
    /// A signature/claims failure verifying an upstream id_token.
    Oidc(OidcError),
    /// A storage-layer failure.
    Store(StoreError),
}

impl From<StoreError> for TokenError {
    fn from(err: StoreError) -> Self {
        TokenError::Store(err)
    }
}

impl From<OidcError> for TokenError {
    fn from(err: OidcError) -> Self {
        TokenError::Oidc(err)
    }
}

/// Verify a PKCE `code_verifier` against the `code_challenge` stored at
/// `/v1/oauth/authorize` time. S256 only — ADR 0054 mandates it for every
/// redirect-based flow; a `plain` challenge method is rejected outright
/// rather than accepted as a weaker fallback.
pub fn verify_pkce(code_verifier: &str, code_challenge: &str, code_challenge_method: &str) -> bool {
    if code_challenge_method != "S256" {
        return false;
    }
    let digest = Sha256::digest(code_verifier.as_bytes());
    let computed = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    computed == code_challenge
}

/// Mint a fresh access + refresh token pair for an already-bound identity.
/// The only caller-trusted input is `oauth_identity_id` — every path that
/// reaches here has already verified the identity is genuinely bound
/// (Path A via `redeem_authorization_code`, Path B via `redeem_identity_token`).
pub async fn mint_token_pair(
    store: &Store,
    runtime: &OAuthRuntime,
    oauth_identity_id: Uuid,
    provider: &str,
    client_id: &str,
) -> Result<TokenPair, TokenError> {
    let now = Utc::now();
    let access_expires_at = now + runtime.access_token_ttl;
    let issued = store
        .issue_oauth_token(
            &format!("oauth:{provider}:{client_id}"),
            access_expires_at,
            provider,
            oauth_identity_id,
            client_id,
        )
        .await?;

    let refresh_raw = generate_refresh_token();
    let refresh_hash = hash_secret(&refresh_raw);
    let refresh_expires_at = now + runtime.refresh_token_ttl;
    store
        .issue_refresh_token(
            &refresh_hash,
            oauth_identity_id,
            client_id,
            refresh_expires_at,
        )
        .await?;

    Ok(TokenPair {
        access_token: issued.token,
        refresh_token: refresh_raw,
        expires_in: runtime.access_token_ttl.as_secs(),
    })
}

/// Redeem an axon-minted authorization code (the tail of Path A):
/// verifies PKCE and that `client_id`/`redirect_uri` match what was recorded
/// at `/v1/oauth/authorize` time, then mints a token pair.
pub async fn redeem_authorization_code(
    store: &Store,
    runtime: &OAuthRuntime,
    code: &str,
    code_verifier: &str,
    client_id: &str,
    redirect_uri: &str,
) -> Result<TokenPair, TokenError> {
    let code_hash = hash_secret(code);
    let request = store
        .redeem_authorization_code(&code_hash)
        .await?
        .ok_or(TokenError::InvalidGrant("unknown or already-used code"))?;

    if request.client_id != client_id || request.redirect_uri != redirect_uri {
        return Err(TokenError::InvalidGrant("client_id/redirect_uri mismatch"));
    }
    if !verify_pkce(
        code_verifier,
        &request.code_challenge,
        &request.code_challenge_method,
    ) {
        return Err(TokenError::InvalidGrant("PKCE verification failed"));
    }
    let oauth_identity_id = request.oauth_identity_id.ok_or(TokenError::InvalidGrant(
        "flow never completed upstream verification",
    ))?;

    mint_token_pair(
        store,
        runtime,
        oauth_identity_id,
        &request.provider,
        client_id,
    )
    .await
}

/// Redeem a refresh token: single-use with rotation. On reuse of an
/// already-rotated token, [`Store::redeem_refresh_token`] has already revoked
/// every other active refresh token for that identity+client as a side
/// effect — this just surfaces the rejection.
pub async fn redeem_refresh_token(
    store: &Store,
    runtime: &OAuthRuntime,
    raw_refresh_token: &str,
) -> Result<TokenPair, TokenError> {
    let old_hash = hash_secret(raw_refresh_token);
    let new_id = Uuid::new_v4();
    let new_raw = generate_refresh_token();
    let new_hash = hash_secret(&new_raw);
    let new_expires_at = Utc::now() + runtime.refresh_token_ttl;

    let rotated = store
        .redeem_refresh_token(&old_hash, new_id, &new_hash, new_expires_at)
        .await?;

    let rotated = match rotated {
        Ok(rotated) => rotated,
        Err(RedeemRefreshTokenError::NotFound) => {
            return Err(TokenError::InvalidGrant("unknown refresh token"))
        }
        Err(RedeemRefreshTokenError::Expired) => {
            return Err(TokenError::InvalidGrant("refresh token expired"))
        }
        Err(RedeemRefreshTokenError::Reused) => {
            return Err(TokenError::InvalidGrant(
                "refresh token already used; all sessions for this client revoked",
            ))
        }
    };

    let identity = store
        .find_identity_by_id(rotated.oauth_identity_id)
        .await?
        .ok_or(TokenError::NotBound)?;

    let access_expires_at = Utc::now() + runtime.access_token_ttl;
    let issued = store
        .issue_oauth_token(
            &format!("oauth:{}:{}", identity.provider, rotated.client_id),
            access_expires_at,
            &identity.provider,
            identity.id,
            &rotated.client_id,
        )
        .await?;

    Ok(TokenPair {
        access_token: issued.token,
        refresh_token: new_raw,
        expires_in: runtime.access_token_ttl.as_secs(),
    })
}

/// Redeem a Path B native identity token directly: verify it against the
/// named provider, enforce single-use replay consumption, confirm the
/// asserted subject is already bound, then mint a token pair. `client_id` is
/// **not** a security boundary here (unlike Path A) — trust rests entirely
/// on the provider's signature plus the bound `sub` match.
pub async fn redeem_identity_token(
    store: &Store,
    runtime: &OAuthRuntime,
    provider_name: &str,
    raw_identity_token: &str,
    client_id: &str,
) -> Result<TokenPair, TokenError> {
    let provider = runtime
        .provider(provider_name)
        .ok_or(TokenError::UnknownProvider)?;
    let verified = provider
        .verify_identity_token(raw_identity_token, None)
        .await?;

    // Check bound-ness *before* burning the replay key: `NotBound` is a
    // recoverable, client-retriable condition (the client may simply be
    // racing `axon oauth bind`), and the provider's signature/expiry already
    // prevent forgery/replay independent of binding — so there is no
    // security reason to consume a token's single-use slot on a rejection
    // that isn't itself a replay.
    let _identity = store
        .find_identity(provider_name, &verified.subject)
        .await?
        .ok_or(TokenError::NotBound)?;

    let pair = store
        .redeem_identity_atomically(
            &axon_store::IdentityRedemption {
                provider: provider_name,
                subject: &verified.subject,
                email: verified.email.as_deref(),
                replay_key: &verified.replay_key,
                client_id,
                access_expires_at: Utc::now() + runtime.access_token_ttl,
                refresh_expires_at: Utc::now() + runtime.refresh_token_ttl,
            },
            None,
        )
        .await?
        .ok_or(TokenError::InvalidGrant(
            "identity unavailable or token already used",
        ))?;
    Ok(TokenPair {
        access_token: pair.access_token,
        refresh_token: pair.refresh_token,
        expires_in: runtime.access_token_ttl.as_secs(),
    })
}

/// Generate a fresh opaque, high-entropy value: 256 bits of CSPRNG entropy,
/// base64url-encoded without padding. The shared primitive behind refresh
/// tokens, the axon-minted authorization code, and the upstream
/// `state`/`nonce` pair — all of which just need to be unguessable, not
/// structured. Delegates to `axon_core::generate_opaque_secret`, the same
/// construction `axon-store::tokens` uses for access tokens, so the two
/// don't carry independent copies of the same primitive.
pub(crate) fn generate_opaque_value() -> String {
    axon_core::generate_opaque_secret()
}

/// Generate a fresh refresh-token secret: an `axon_rt_` prefix over
/// [`generate_opaque_value`].
fn generate_refresh_token() -> String {
    format!("axon_rt_{}", generate_opaque_value())
}

/// Hash a high-entropy secret (refresh token or authorization code) for
/// storage/lookup: SHA-256, base64 — a plain hash suffices, same reasoning as
/// `axon-store::tokens::hash_token`. Delegates to `axon_core::hash_secret`.
pub(crate) fn hash_secret(raw: &str) -> String {
    axon_core::hash_secret(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_round_trips() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        // Precomputed BASE64URL(SHA256(verifier)) — RFC 7636 Appendix B's
        // published test vector.
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(verify_pkce(verifier, challenge, "S256"));
        assert!(!verify_pkce("wrong-verifier", challenge, "S256"));
    }

    #[test]
    fn plain_challenge_method_is_rejected() {
        // ADR 0054 mandates S256 only; `plain` must never be accepted even if
        // the challenge happens to equal the verifier.
        assert!(!verify_pkce("abc", "abc", "plain"));
    }

    #[test]
    fn generated_refresh_tokens_are_prefixed_and_unique() {
        let a = generate_refresh_token();
        let b = generate_refresh_token();
        assert!(a.starts_with("axon_rt_"));
        assert_ne!(a, b);
    }
}
