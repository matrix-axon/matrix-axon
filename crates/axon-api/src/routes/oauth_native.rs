//! Keyless Apple native login and explicitly authorized owner binding.
use super::oauth::{token_error_into_response, token_success_response, OAuthForm};
use crate::{
    oauth::{
        tokens::{TokenError, TokenPair},
        OAuthRuntime,
    },
    response::ApiError,
    state::BootstrapConfig,
};
use axon_store::{IdentityRedemption, NativeChallenge, Store};
use axum::{
    extract::{ConnectInfo, State},
    http::HeaderMap,
    response::Response,
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc};

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NativeChallengeRequest {
    pub client_id: String,
    /// login, bind (existing bearer), or bootstrap (explicit setup capability).
    pub purpose: String,
    pub bootstrap_code: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct NativeChallengeResponse {
    /// Client-held secret. Never pass it to Apple or persist it.
    pub challenge: String,
    /// Set ASAuthorizationAppleIDRequest.nonce to this exact string. Do not hash again.
    pub nonce: String,
    pub expires_in: u64,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NativeTokenRequest {
    pub client_id: String,
    pub challenge: String,
    pub identity_token: String,
    pub bootstrap_code: Option<String>,
}

fn enabled(runtime: Option<Arc<OAuthRuntime>>) -> Result<Arc<OAuthRuntime>, ApiError> {
    runtime
        .filter(|r| r.native_apple.is_some())
        .ok_or_else(|| ApiError::not_found("native Apple sign-in is disabled"))
}

fn valid_client(runtime: &OAuthRuntime, client: &str) -> Result<(), ApiError> {
    if client.len() > 256 || !runtime.clients.contains_key(client) {
        tracing::warn!(
            provider = "apple",
            reason = "native_unknown_client",
            "Native challenge rejected"
        );
        return Err(ApiError::bad_request("unknown client_id"));
    }
    Ok(())
}

enum AuthorityError {
    Rejected(ApiError),
    Store(axon_store::StoreError),
}

impl From<ApiError> for AuthorityError {
    fn from(error: ApiError) -> Self {
        Self::Rejected(error)
    }
}

impl From<axon_store::StoreError> for AuthorityError {
    fn from(error: axon_store::StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<AuthorityError> for ApiError {
    fn from(error: AuthorityError) -> Self {
        match error {
            AuthorityError::Rejected(error) => error,
            AuthorityError::Store(_) => {
                tracing::error!(
                    provider = "apple",
                    reason = "store_failure",
                    "Native authorization unavailable"
                );
                ApiError::internal()
            }
        }
    }
}

impl From<AuthorityError> for TokenError {
    fn from(error: AuthorityError) -> Self {
        match error {
            AuthorityError::Rejected(_) => Self::InvalidGrant("native owner authorization invalid"),
            AuthorityError::Store(error) => Self::Store(error),
        }
    }
}

async fn authority(
    store: &Store,
    purpose: &str,
    headers: &HeaderMap,
    bootstrap: &Option<BootstrapConfig>,
    code: Option<&str>,
    peer: SocketAddr,
) -> Result<Option<String>, AuthorityError> {
    match purpose {
        "login" if code.is_none() => Ok(None),
        "bind" if code.is_none() => {
            let raw = crate::auth::bearer_from_headers(headers)
                .ok_or_else(|| ApiError::forbidden("an active owner bearer is required"))?;
            if raw.len() > 512 || store.verify_token(raw).await?.is_none() {
                return Err(ApiError::forbidden("an active owner bearer is required").into());
            }
            Ok(Some(axon_core::hash_secret(raw)))
        }
        "bootstrap" => {
            let bootstrap = bootstrap
                .as_ref()
                .ok_or_else(|| ApiError::forbidden("bootstrap is not armed"))?;
            if !bootstrap.allow_remote && !peer.ip().is_loopback() {
                return Err(ApiError::forbidden("bootstrap requires loopback").into());
            }
            let code = code
                .filter(|c| c.len() <= 256)
                .ok_or_else(|| ApiError::forbidden("bootstrap capability required"))?;
            bootstrap
                .validate_access_code(code)
                .map_err(|_| ApiError::forbidden("bootstrap capability invalid or locked"))?;
            if !store.first_credential_bootstrap_available().await? {
                return Err(ApiError::conflict("bootstrap is no longer available").into());
            }
            Ok(Some(axon_core::hash_secret(code)))
        }
        _ => Err(ApiError::bad_request("invalid native flow purpose or authorization").into()),
    }
}

#[utoipa::path(post, path="/v1/oauth/apple/native/challenge",
    request_body(content=NativeChallengeRequest, content_type="application/x-www-form-urlencoded"),
    responses((status=200, body=NativeChallengeResponse), (status=400, description="Invalid request"),
        (status=403, description="Owner authorization required"), (status=404, description="Native Apple disabled"),
        (status=409, description="Bootstrap closed"), (status=429, description="Challenge capacity or rate exceeded")),
    tag="oauth", security())]
pub async fn challenge(
    State(store): State<Store>,
    State(runtime): State<Option<Arc<OAuthRuntime>>>,
    State(bootstrap): State<Option<BootstrapConfig>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    OAuthForm(body): OAuthForm<NativeChallengeRequest>,
) -> Result<Json<NativeChallengeResponse>, ApiError> {
    let runtime = enabled(runtime)?;
    valid_client(&runtime, &body.client_id)?;
    let authority_hash = authority(
        &store,
        &body.purpose,
        &headers,
        &bootstrap,
        body.bootstrap_code.as_deref(),
        peer,
    )
    .await
    .inspect_err(|_| {
        tracing::warn!(
            provider = "apple",
            reason = "native_authorization_rejected",
            "Native challenge rejected"
        );
    })?;
    let secret = axon_core::generate_opaque_secret();
    // Independent public nonce and private redemption capability. The server
    // stores the expected signed claim; clients cannot choose/override it.
    let nonce = axon_core::hash_secret(&axon_core::generate_opaque_secret());
    let c = NativeChallenge {
        hash: axon_core::hash_secret(&secret),
        purpose: body.purpose,
        client_id: body.client_id,
        instance: runtime.external_base_url.clone(),
        nonce: nonce.clone(),
        authority_hash,
    };
    if !store.create_native_challenge(&c).await? {
        return Err(ApiError::too_many_requests(
            "too many pending native sign-ins",
        ));
    }
    Ok(Json(NativeChallengeResponse {
        challenge: secret,
        nonce,
        expires_in: 300,
    }))
}

#[utoipa::path(post, path="/v1/oauth/apple/native/token",
    request_body(content=NativeTokenRequest, content_type="application/x-www-form-urlencoded"),
    responses((status=200, body=super::oauth::TokenSuccessBody),
        (status=400, description="Invalid, expired, unbound, or replayed identity", body=super::oauth::OAuthErrorBody),
        (status=403, description="Owner authorization invalid"), (status=404, description="Native Apple disabled")),
    tag="oauth", security())]
pub async fn token(
    State(store): State<Store>,
    State(runtime): State<Option<Arc<OAuthRuntime>>>,
    State(bootstrap): State<Option<BootstrapConfig>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    OAuthForm(body): OAuthForm<NativeTokenRequest>,
) -> Result<Response, ApiError> {
    let runtime = enabled(runtime)?;
    if valid_client(&runtime, &body.client_id).is_err() {
        return Ok(super::oauth::oauth_error_response(
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_request",
            "Unknown native client. Register this client on the Axon server.",
        ));
    }
    // Always key on the flow capability, independent of form-field ordering
    // or a caller supplying a different (invalid) JWT on every attempt.
    if !runtime.rate_limiter.check_key(&body.challenge) {
        return Err(ApiError::too_many_requests(
            "native challenge rate limit exceeded",
        ));
    }
    let result = async {
        if body.challenge.len() != 43 || body.identity_token.len() > 16384 {
            return Err(TokenError::InvalidGrant("native input invalid"));
        }
        let c = store
            .native_challenge(&axon_core::hash_secret(&body.challenge))
            .await?
            .ok_or(TokenError::InvalidGrant("native challenge unavailable"))?;
        if c.client_id != body.client_id || c.instance != runtime.external_base_url {
            return Err(TokenError::InvalidGrant(
                "native challenge context mismatch",
            ));
        }
        let authorization = authority(
            &store,
            &c.purpose,
            &headers,
            &bootstrap,
            body.bootstrap_code.as_deref(),
            peer,
        )
        .await
        .map_err(TokenError::from)?;
        if c.authority_hash != authorization {
            return Err(TokenError::InvalidGrant(
                "native owner authorization changed",
            ));
        }
        let verified = runtime
            .native_apple
            .as_ref()
            .expect("enabled native verifier")
            .verify(&body.identity_token, &c.nonce)
            .await?;
        if c.purpose == "login"
            && store
                .find_identity("apple", &verified.subject)
                .await?
                .is_none()
        {
            return Err(TokenError::NotBound);
        }
        let pair = store
            .redeem_identity_atomically(
                &IdentityRedemption {
                    provider: "apple",
                    subject: &verified.subject,
                    email: verified.email.as_deref(),
                    replay_key: &verified.replay_key,
                    client_id: &body.client_id,
                    access_expires_at: Utc::now() + runtime.access_token_ttl,
                    refresh_expires_at: Utc::now() + runtime.refresh_token_ttl,
                },
                Some(&c),
            )
            .await?
            .ok_or(TokenError::InvalidGrant(
                "native challenge no longer redeemable",
            ))?;
        Ok(TokenPair {
            access_token: pair.access_token,
            refresh_token: pair.refresh_token,
            expires_in: runtime.access_token_ttl.as_secs(),
        })
    }
    .await;
    Ok(match result {
        Ok(pair) => token_success_response(pair),
        Err(e) => token_error_into_response(e),
    })
}

pub async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert("referrer-policy", "no-referrer".parse().unwrap());
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::StatusCode, response::IntoResponse};

    #[test]
    fn database_outages_are_not_reported_as_rejected_credentials() {
        let failure =
            || AuthorityError::Store(axon_store::StoreError::Sqlx(sqlx_core::Error::PoolClosed));
        assert_eq!(
            ApiError::from(failure()).into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            token_error_into_response(failure().into()).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        let rejected = AuthorityError::Rejected(ApiError::forbidden("owner required"));
        assert_eq!(
            token_error_into_response(rejected.into()).status(),
            StatusCode::BAD_REQUEST
        );
    }
}
