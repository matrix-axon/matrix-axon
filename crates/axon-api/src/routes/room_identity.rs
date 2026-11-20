use std::sync::Arc;

use axon_core::IdentityChangeKind;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use uuid::Uuid;

use crate::{response::ApiError, ResolveIdentityChange, RoomIdentityService};

/// Accept a changed identity
///
/// Acknowledge the changed identity of a user who was never verified.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/users/{user_id}/identity/accept",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("user_id" = String, Path, description = "Matrix user id"),
    ),
    request_body = ResolveIdentityChange,
    responses(
        (status = 204, description = "Identity accepted across the account's rooms"),
        (status = 400, description = "Invalid user id or fingerprint"),
        (status = 404, description = "Account or user identity not found"),
        (status = 409, description = "Account not ready or warning changed"),
        (status = 500, description = "Local identity acknowledgement failed"),
        (status = 503, description = "Room identity service unavailable", body = crate::ErrorResponse),
    ),
    tag = "verification",
)]
pub async fn accept(
    State(service): State<Option<Arc<dyn RoomIdentityService>>>,
    Path((account_id, user_id)): Path<(Uuid, String)>,
    Json(input): Json<ResolveIdentityChange>,
) -> Result<StatusCode, ApiError> {
    resolve(
        service,
        account_id,
        &user_id,
        &input,
        IdentityChangeKind::PinViolation,
    )
    .await
}

/// Withdraw verification
///
/// Accept the changed identity as unverified.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/users/{user_id}/identity/withdraw_verification",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("user_id" = String, Path, description = "Matrix user id"),
    ),
    request_body = ResolveIdentityChange,
    responses(
        (status = 204, description = "Verification withdrawn across the account's rooms"),
        (status = 400, description = "Invalid user id or fingerprint"),
        (status = 404, description = "Account or user identity not found"),
        (status = 409, description = "Account not ready or warning changed"),
        (status = 500, description = "Local identity acknowledgement failed"),
        (status = 503, description = "Room identity service unavailable", body = crate::ErrorResponse),
    ),
    tag = "verification",
)]
pub async fn withdraw_verification(
    State(service): State<Option<Arc<dyn RoomIdentityService>>>,
    Path((account_id, user_id)): Path<(Uuid, String)>,
    Json(input): Json<ResolveIdentityChange>,
) -> Result<StatusCode, ApiError> {
    resolve(
        service,
        account_id,
        &user_id,
        &input,
        IdentityChangeKind::VerificationViolation,
    )
    .await
}

async fn resolve(
    service: Option<Arc<dyn RoomIdentityService>>,
    account_id: Uuid,
    user_id: &str,
    input: &ResolveIdentityChange,
    kind: IdentityChangeKind,
) -> Result<StatusCode, ApiError> {
    let service = service
        .ok_or_else(|| ApiError::service_unavailable("room identity service unavailable"))?;
    service
        .resolve(account_id, user_id, &input.fingerprint, kind)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
