//! The management API (ADR 0109): administering this instance from a client.
//!
//! Everything here lives under `/v1/management/`, behind two layers assembled
//! in [`router`](crate::router): the bearer gate every `/v1/` route shares,
//! then [`require_enabled`], the operator's `[server] management_api` switch.
//!
//! Reads need only that. A handler that changes credentials also takes a
//! [`CredentialChange`], whose extraction is the step-up check, so the rule
//! cannot be forgotten by a handler that compiles.

use std::sync::Arc;

use axon_store::{IdentityRemoval, Store, TokenRevocation};
use axum::extract::{Request, State};
use axum::http::header::{CACHE_CONTROL, REFERRER_POLICY};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use serde::Deserialize;
use utoipa::IntoParams;
use uuid::Uuid;

use crate::auth::{AuthedToken, CredentialChange};
use crate::dto::{
    BindDto, ManagementTokenDto, MintTokenRequest, MintedTokenDto, OauthIdentityDto,
    StartBindRequest, StartedBindDto,
};
use crate::extract::{Json, Path, Query};
use crate::oauth::bind::BindRefusal;
use crate::oauth::OAuthRuntime;
use crate::response::{ApiError, ApiResponse};
use crate::state::ManagementConfig;

/// Cap on a management request body, applied before it is parsed. The largest
/// legitimate one is a token label.
pub(crate) const MAX_BODY_BYTES: usize = 4 * 1024;

/// The longest token label, in characters. Shared with the first-run
/// bootstrap, which mints the same kind of token.
pub(crate) const TOKEN_LABEL_MAX_CHARS: usize = 80;

/// The most tokens one list response carries. One human creates few tokens on
/// purpose, but every hour of a signed-in session leaves an expired access
/// token behind, so the table itself is not bounded by that.
const TOKEN_LIST_LIMIT: i64 = 500;

/// The operator's switch. With `[server] management_api = false` every
/// management route answers `403 management_disabled`, authenticated or not
/// in the sense that matters: the bearer gate has already run.
pub async fn require_enabled(
    State(management): State<ManagementConfig>,
    req: Request,
    next: Next,
) -> Response {
    if !management.enabled {
        return ApiError::management_disabled().into_response();
    }
    next.run(req).await
}

/// The providers a sign-in could currently go through: what makes a bound
/// identity a credential the owner can actually use.
fn sign_in_providers(oauth: Option<&OAuthRuntime>) -> Vec<String> {
    oauth
        .map(OAuthRuntime::sign_in_providers)
        .unwrap_or_default()
}

/// List the upstream sign-in identities bound to this instance's owner.
///
/// Each carries `current`, set on the identity the calling session signed in
/// with, and `sign_in_available`, false when its provider is switched off and
/// the identity therefore cannot produce a session.
#[utoipa::path(
    get,
    path = "/v1/management/oauth/identities",
    responses(
        (status = 200, description = "The bound identities, most recently linked first", body = ApiResponse<Vec<OauthIdentityDto>>),
        (status = 403, description = "The management API is disabled (`management_disabled`)", body = crate::response::ErrorResponse),
    ),
    tag = "management",
)]
pub async fn list_identities(
    State(store): State<Store>,
    State(oauth): State<Option<Arc<OAuthRuntime>>>,
    Extension(caller): Extension<AuthedToken>,
) -> Result<ApiResponse<Vec<OauthIdentityDto>>, ApiError> {
    let providers = sign_in_providers(oauth.as_deref());
    let identities = store
        .list_identities()
        .await?
        .into_iter()
        .map(|identity| OauthIdentityDto::new(identity, &caller, &providers))
        .collect();
    Ok(ApiResponse::new(identities))
}

/// Query parameters for unbinding an identity.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct UnbindQuery {
    /// Go ahead even if this is the last way to sign in. Without it such a
    /// request is refused with `409 last_credential` and changes nothing.
    #[serde(default)]
    pub allow_lockout: bool,
}

/// Unbind a sign-in identity and end every session it was used to start.
///
/// Axon forgets the identity: its access tokens are revoked and its refresh
/// tokens deleted, atomically. This does **not** revoke the upstream
/// provider's own authorization of Axon; that is the provider's to withdraw.
///
/// A credential change, so it needs a non-expiring token or a session whose
/// interactive sign-in was in the last ten minutes (`403
/// recent_sign_in_required` otherwise). Unbinding the identity the calling
/// session signed in with ends that session too.
#[utoipa::path(
    delete,
    path = "/v1/management/oauth/identities/{identity_id}",
    params(
        ("identity_id" = Uuid, Path, description = "The identity's id, from the list"),
        UnbindQuery,
    ),
    responses(
        (status = 204, description = "The identity was unbound"),
        (status = 403, description = "`management_disabled`, or `recent_sign_in_required`: sign in again and retry", body = crate::response::ErrorResponse),
        (status = 404, description = "No such identity", body = crate::response::ErrorResponse),
        (status = 409, description = "`last_credential`: this is the last way to sign in. Nothing was changed; repeat with `allow_lockout=true` to go ahead", body = crate::response::ErrorResponse),
    ),
    tag = "management",
)]
pub async fn unbind_identity(
    State(store): State<Store>,
    State(oauth): State<Option<Arc<OAuthRuntime>>>,
    CredentialChange(caller): CredentialChange,
    Path(identity_id): Path<Uuid>,
    Query(query): Query<UnbindQuery>,
) -> Result<StatusCode, ApiError> {
    let removal = if query.allow_lockout {
        match store.delete_identity(identity_id).await? {
            true => IdentityRemoval::Removed,
            false => IdentityRemoval::NotFound,
        }
    } else {
        let providers = sign_in_providers(oauth.as_deref());
        store
            .delete_identity_unless_last_credential(identity_id, &providers)
            .await?
    };
    match removal {
        IdentityRemoval::Removed if query.allow_lockout => {
            // The one path that skips the lockout guard, and so the one that
            // can leave the owner with no way in: worth more than an info line.
            tracing::warn!(
                acting_token_id = %caller.id,
                %identity_id,
                allow_lockout = true,
                "OAuth identity unbound through the management API with the lockout guard overridden"
            );
            Ok(StatusCode::NO_CONTENT)
        }
        IdentityRemoval::Removed => {
            tracing::info!(
                acting_token_id = %caller.id,
                %identity_id,
                allow_lockout = false,
                "OAuth identity unbound through the management API"
            );
            Ok(StatusCode::NO_CONTENT)
        }
        IdentityRemoval::NotFound => Err(ApiError::not_found("no such identity")),
        IdentityRemoval::LastCredential => {
            tracing::info!(
                acting_token_id = %caller.id,
                %identity_id,
                reason = "last_credential",
                "OAuth identity unbind refused"
            );
            Err(ApiError::last_credential())
        }
    }
}

/// List this instance's bearer tokens, without their secrets.
///
/// Revoked and expired tokens are included, so an entry the owner does not
/// recognize stays visible after it stops working. The tokens that still work
/// come first, then the rest, newest first within each; at most 500 are
/// returned, which only ever drops the oldest dead ones. `current` marks the
/// token that made this request.
#[utoipa::path(
    get,
    path = "/v1/management/tokens",
    responses(
        (status = 200, description = "The tokens, working ones first", body = ApiResponse<Vec<ManagementTokenDto>>),
        (status = 403, description = "The management API is disabled (`management_disabled`)", body = crate::response::ErrorResponse),
    ),
    tag = "management",
)]
pub async fn list_tokens(
    State(store): State<Store>,
    Extension(caller): Extension<AuthedToken>,
) -> Result<ApiResponse<Vec<ManagementTokenDto>>, ApiError> {
    let tokens = store
        .list_tokens_live_first(TOKEN_LIST_LIMIT)
        .await?
        .into_iter()
        .map(|token| ManagementTokenDto::new(token, &caller))
        .collect();
    Ok(ApiResponse::new(tokens))
}

/// A label fit to store and to print: trimmed, one to
/// [`TOKEN_LABEL_MAX_CHARS`] characters, no control characters. The CLI
/// prints labels to a terminal, so an escape sequence in one is not harmless.
fn validate_label(label: &str) -> Result<&str, ApiError> {
    let label = label.trim();
    if label.is_empty() {
        return Err(ApiError::bad_request("label must not be empty"));
    }
    if label.chars().count() > TOKEN_LABEL_MAX_CHARS {
        return Err(ApiError::bad_request(format!(
            "label must be at most {TOKEN_LABEL_MAX_CHARS} characters"
        )));
    }
    if label.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "label must not contain control characters",
        ));
    }
    Ok(label)
}

/// Mint a bearer token that never expires.
///
/// The response is the only time the token's secret is available: Axon keeps
/// its hash and cannot show it again. It is sent with `Cache-Control:
/// no-store`.
///
/// A credential change, so it needs a non-expiring token or a session whose
/// interactive sign-in was in the last ten minutes (`403
/// recent_sign_in_required` otherwise). The new token records which token
/// minted it.
#[utoipa::path(
    post,
    path = "/v1/management/tokens",
    request_body = MintTokenRequest,
    responses(
        (status = 201, description = "The new token, with its secret, once", body = ApiResponse<MintedTokenDto>),
        (status = 400, description = "The label is empty, over 80 characters, or contains control characters", body = crate::response::ErrorResponse),
        (status = 403, description = "`management_disabled`, or `recent_sign_in_required`: sign in again and retry", body = crate::response::ErrorResponse),
        (status = 413, description = "The request body is over 4 KiB", body = crate::response::ErrorResponse),
    ),
    tag = "management",
)]
pub async fn mint_token(
    State(store): State<Store>,
    CredentialChange(caller): CredentialChange,
    Json(body): Json<MintTokenRequest>,
) -> Result<Response, ApiError> {
    let label = validate_label(&body.label)?;
    let issued = store.issue_token_created_by(label, caller.id).await?;
    // The ids, never the secret.
    tracing::info!(
        acting_token_id = %caller.id,
        token_id = %issued.id,
        "Token minted through the management API"
    );
    let mut response = (
        StatusCode::CREATED,
        ApiResponse::new(MintedTokenDto {
            id: issued.id,
            label: issued.label,
            token: issued.token,
        }),
    )
        .into_response();
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    Ok(response)
}

/// Query parameters for revoking a token.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct RevokeQuery {
    /// Go ahead even if this is the last way to sign in. Without it such a
    /// request is refused with `409 last_credential` and changes nothing.
    #[serde(default)]
    pub allow_lockout: bool,
}

/// Revoke a token. It stops working at once and stays in the list.
///
/// Revoking the token that made the request is allowed: it is how a device
/// signs itself out. Revoking a token that is already revoked succeeds and
/// changes nothing.
///
/// A credential change, so it needs a non-expiring token or a session whose
/// interactive sign-in was in the last ten minutes (`403
/// recent_sign_in_required` otherwise).
#[utoipa::path(
    delete,
    path = "/v1/management/tokens/{token_id}",
    params(
        ("token_id" = Uuid, Path, description = "The token's id, from the list"),
        RevokeQuery,
    ),
    responses(
        (status = 204, description = "The token is revoked"),
        (status = 403, description = "`management_disabled`, or `recent_sign_in_required`: sign in again and retry", body = crate::response::ErrorResponse),
        (status = 404, description = "No such token", body = crate::response::ErrorResponse),
        (status = 409, description = "`last_credential`: this is the last way to sign in. Nothing was changed; repeat with `allow_lockout=true` to go ahead", body = crate::response::ErrorResponse),
    ),
    tag = "management",
)]
pub async fn revoke_token(
    State(store): State<Store>,
    State(oauth): State<Option<Arc<OAuthRuntime>>>,
    CredentialChange(caller): CredentialChange,
    Path(token_id): Path<Uuid>,
    Query(query): Query<RevokeQuery>,
) -> Result<StatusCode, ApiError> {
    let revocation = if query.allow_lockout {
        store.revoke_token_allowing_lockout(token_id).await?
    } else {
        let providers = sign_in_providers(oauth.as_deref());
        store
            .revoke_token_unless_last_credential(token_id, &providers)
            .await?
    };
    match revocation {
        TokenRevocation::Revoked if query.allow_lockout => {
            // As with unbind: the one path that can leave the owner locked out.
            tracing::warn!(
                acting_token_id = %caller.id,
                %token_id,
                allow_lockout = true,
                "Token revoked through the management API with the lockout guard overridden"
            );
            Ok(StatusCode::NO_CONTENT)
        }
        TokenRevocation::Revoked => {
            tracing::info!(
                acting_token_id = %caller.id,
                %token_id,
                allow_lockout = false,
                "Token revoked through the management API"
            );
            Ok(StatusCode::NO_CONTENT)
        }
        TokenRevocation::AlreadyRevoked => Ok(StatusCode::NO_CONTENT),
        TokenRevocation::NotFound => Err(ApiError::not_found("no such token")),
        TokenRevocation::LastCredential => {
            tracing::info!(
                acting_token_id = %caller.id,
                %token_id,
                reason = "last_credential",
                "Token revoke refused"
            );
            Err(ApiError::last_credential())
        }
    }
}

/// Start binding a new sign-in identity to this instance's owner.
///
/// Returns a `url` to open in a browser. Signing in there with the provider
/// binds that identity; poll the bind by its `id` to learn when. The bind
/// lapses after ten minutes.
///
/// A credential change, so it needs a non-expiring token or a session whose
/// interactive sign-in was in the last ten minutes (`403
/// recent_sign_in_required` otherwise).
#[utoipa::path(
    post,
    path = "/v1/management/oauth/binds",
    request_body = StartBindRequest,
    responses(
        (status = 201, description = "The bind was started", body = ApiResponse<StartedBindDto>),
        (status = 400, description = "Unknown provider", body = crate::response::ErrorResponse),
        (status = 403, description = "`management_disabled`, or `recent_sign_in_required`: sign in again and retry", body = crate::response::ErrorResponse),
        (status = 409, description = "`bind_unavailable`: OAuth is off on this server, or the provider is not enabled for browser sign-in", body = crate::response::ErrorResponse),
        (status = 413, description = "The request body is over 4 KiB", body = crate::response::ErrorResponse),
    ),
    tag = "management",
)]
pub async fn start_bind(
    State(store): State<Store>,
    State(oauth): State<Option<Arc<OAuthRuntime>>>,
    CredentialChange(caller): CredentialChange,
    Json(body): Json<StartBindRequest>,
) -> Result<Response, ApiError> {
    let target = OAuthRuntime::bind_target(oauth.as_deref(), &body.provider).map_err(
        |refusal| match refusal {
            BindRefusal::UnknownProvider => ApiError::bad_request(refusal.to_string()),
            _ => ApiError::bind_unavailable(refusal.to_string()),
        },
    )?;
    let started = target.start(&store).await?;
    tracing::info!(
        acting_token_id = %caller.id,
        bind_id = %started.request.device_code,
        provider = target.provider(),
        "OAuth identity bind started through the management API"
    );
    let mut response = (
        StatusCode::CREATED,
        ApiResponse::new(StartedBindDto {
            bind: BindDto::new(started.request, chrono::Utc::now()),
            url: started.url,
        }),
    )
        .into_response();
    // The URL carries the code that drives the bind: not for a cache.
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    Ok(response)
}

/// Read where an identity bind stands: `pending`, `completed` or `expired`.
///
/// A bind's record is removed some time after it lapses, so a `404` for an id
/// this server issued means the same as `expired`.
#[utoipa::path(
    get,
    path = "/v1/management/oauth/binds/{bind_id}",
    params(
        ("bind_id" = Uuid, Path, description = "The bind's id, from starting it"),
    ),
    responses(
        (status = 200, description = "The bind's status", body = ApiResponse<BindDto>),
        (status = 403, description = "The management API is disabled (`management_disabled`)", body = crate::response::ErrorResponse),
        (status = 404, description = "No such bind, or it lapsed and was removed", body = crate::response::ErrorResponse),
    ),
    tag = "management",
)]
pub async fn bind_status(
    State(store): State<Store>,
    Path(bind_id): Path<Uuid>,
) -> Result<ApiResponse<BindDto>, ApiError> {
    let request = store
        .find_bind_request(bind_id)
        .await?
        .ok_or_else(|| ApiError::not_found("no such bind"))?;
    Ok(ApiResponse::new(BindDto::new(request, chrono::Utc::now())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_is_trimmed_and_bounded() {
        assert_eq!(validate_label("  laptop  ").unwrap(), "laptop");
        assert!(validate_label("").is_err());
        assert!(validate_label(" \t ").is_err());
        let longest = "é".repeat(TOKEN_LABEL_MAX_CHARS);
        assert_eq!(validate_label(&longest).unwrap(), longest);
        assert!(validate_label(&"é".repeat(TOKEN_LABEL_MAX_CHARS + 1)).is_err());
    }

    #[test]
    fn a_label_cannot_carry_terminal_escapes() {
        assert!(validate_label("phone\u{1b}[2J").is_err());
        assert!(validate_label("two\nlines").is_err());
    }
}
