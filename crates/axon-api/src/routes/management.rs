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

use axon_store::{IdentityRemoval, Store};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use serde::Deserialize;
use utoipa::IntoParams;
use uuid::Uuid;

use crate::auth::{AuthedToken, CredentialChange};
use crate::dto::OauthIdentityDto;
use crate::extract::{Path, Query};
use crate::oauth::OAuthRuntime;
use crate::response::{ApiError, ApiResponse};
use crate::state::ManagementConfig;

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
