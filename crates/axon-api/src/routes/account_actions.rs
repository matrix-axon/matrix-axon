//! Account-action routes (ADR 0068, M19f): this account's own display name
//! and avatar, reading any user's profile, this account's ignore list, and
//! public-room directory search.
//!
//! Every handler routes through the [`AccountActionsSender`] port (injected
//! via `AppState`); none resolve through a `Room` handle — all go straight to
//! `ClientManager::get_or_connect` — the same "account-scoped, no room"
//! resolution path [`room_entry`](crate::routes::room_entry) uses. Unlike
//! that batch, two of these seven handlers (`get_user_profile`,
//! `search_public_rooms`) are reads, not mutations, so their success envelope
//! carries real data rather than an empty object — the same mix
//! [`power_levels`](crate::routes::power_levels) already has. Every call is
//! logged with `account_id`, the target user/room where applicable, elapsed
//! time, and — on failure — the mapped error, matching every other M19
//! batch's convention.
//!
//! `set_avatar` claims a staged upload from [`StagedUploadService`] the same
//! way [`room_settings::set_room_avatar`](crate::routes::room_settings::set_room_avatar)
//! does — Axon has no route that hands a client an `mxc://` URI to reference
//! directly.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use uuid::Uuid;

use crate::dto::{
    MatrixProfileDto, PublicRoomsPageDto, PublicRoomsQueryDto, SetAccountAvatarRequest,
    SetDisplayNameRequest,
};
use crate::extract::{Json, Path, Query};
use crate::response::{ApiError, ApiResponse};
use crate::sender::AccountActionsSender;
use crate::uploads::StagedUploadService;

/// Hard cap on `search_public_rooms`' page size, regardless of the
/// caller-requested `limit` — mirrors `rooms.rs`/`search.rs`'s `MAX_LIMIT`
/// convention. Needed here even more than on those routes: `server`, below,
/// lets a caller target an arbitrary third-party homeserver's directory, so
/// an uncapped `limit` would let a misbehaving or malicious remote server
/// (not just this account's own homeserver) dictate how much data Axon
/// buffers and returns. `None` (the field's default) is left unclamped —
/// it means "let the homeserver pick its own default," not "unbounded."
const MAX_PUBLIC_ROOMS_LIMIT: u32 = 200;

/// Set the account display name
///
/// Set this account's own display name. An empty `display_name` clears it.
#[utoipa::path(
    put,
    path = "/v1/accounts/{account_id}/profile/display_name",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
    ),
    request_body = SetDisplayNameRequest,
    responses(
        (status = 200, description = "Display name set", body = ApiResponse<serde_json::Value>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 404, description = "Account not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "account-actions",
)]
pub async fn set_display_name(
    State(account_actions): State<Arc<dyn AccountActionsSender>>,
    Path(account_id): Path<Uuid>,
    Json(req): Json<SetDisplayNameRequest>,
) -> Result<ApiResponse<serde_json::Value>, ApiError> {
    let started_at = Instant::now();
    match account_actions
        .set_display_name(account_id, &req.display_name)
        .await
    {
        Ok(()) => {
            tracing::info!(
                account_id = %account_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "account_actions: set_display_name succeeded"
            );
            Ok(ApiResponse::new(serde_json::json!({})))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "account_actions: set_display_name failed"
            );
            Err(err.into())
        }
    }
}

/// Set the account avatar
///
/// Set this account's avatar from an already-staged upload.
#[utoipa::path(
    put,
    path = "/v1/accounts/{account_id}/profile/avatar",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
    ),
    request_body = SetAccountAvatarRequest,
    responses(
        (status = 200, description = "Avatar set", body = ApiResponse<serde_json::Value>),
        (status = 400, description = "Malformed request, or the upload isn't an image", body = crate::response::ErrorResponse),
        (status = 404, description = "Account or upload not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "account-actions",
)]
pub async fn set_account_avatar(
    State(account_actions): State<Arc<dyn AccountActionsSender>>,
    State(uploads): State<Arc<dyn StagedUploadService>>,
    Path(account_id): Path<Uuid>,
    Json(req): Json<SetAccountAvatarRequest>,
) -> Result<ApiResponse<serde_json::Value>, ApiError> {
    let started_at = Instant::now();
    let result = crate::uploads::with_staged_upload(
        uploads.as_ref(),
        account_id,
        req.upload_id,
        "set account avatar",
        |attachment| account_actions.set_avatar(account_id, attachment),
    )
    .await;
    match result {
        Ok(()) => {
            tracing::info!(
                account_id = %account_id,
                upload_id = %req.upload_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "account_actions: set_avatar succeeded"
            );
            Ok(ApiResponse::new(serde_json::json!({})))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                upload_id = %req.upload_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "account_actions: set_avatar failed"
            );
            Err(err)
        }
    }
}

/// Clear the account avatar
///
/// Clear this account's avatar.
#[utoipa::path(
    delete,
    path = "/v1/accounts/{account_id}/profile/avatar",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
    ),
    responses(
        (status = 200, description = "Avatar cleared", body = ApiResponse<serde_json::Value>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 404, description = "Account not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "account-actions",
)]
pub async fn remove_account_avatar(
    State(account_actions): State<Arc<dyn AccountActionsSender>>,
    Path(account_id): Path<Uuid>,
) -> Result<ApiResponse<serde_json::Value>, ApiError> {
    let started_at = Instant::now();
    match account_actions.remove_avatar(account_id).await {
        Ok(()) => {
            tracing::info!(
                account_id = %account_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "account_actions: remove_avatar succeeded"
            );
            Ok(ApiResponse::new(serde_json::json!({})))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "account_actions: remove_avatar failed"
            );
            Err(err.into())
        }
    }
}

/// Get a user profile
///
/// Read a Matrix user's profile (display name + avatar). `user_id` may be
/// this account's own user id or any other user's.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/users/{user_id}/profile",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("user_id" = String, Path, description = "Target Matrix user id (@user:server)"),
    ),
    responses(
        (status = 200, description = "The target user's profile", body = ApiResponse<MatrixProfileDto>),
        (status = 400, description = "user_id is not a syntactically valid Matrix user id", body = crate::response::ErrorResponse),
        (status = 404, description = "Account or user not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "account-actions",
)]
pub async fn get_user_profile(
    State(account_actions): State<Arc<dyn AccountActionsSender>>,
    Path((account_id, user_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<MatrixProfileDto>, ApiError> {
    let started_at = Instant::now();
    match account_actions.user_profile(account_id, &user_id).await {
        Ok(profile) => {
            tracing::info!(
                account_id = %account_id,
                user_id = %user_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "account_actions: get_user_profile succeeded"
            );
            Ok(ApiResponse::new(MatrixProfileDto::from_profile(
                user_id, profile,
            )))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                user_id = %user_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "account_actions: get_user_profile failed"
            );
            Err(err.into())
        }
    }
}

/// Ignore a user
///
/// Add `user_id` to this account's ignore list.
#[utoipa::path(
    put,
    path = "/v1/accounts/{account_id}/users/{user_id}/ignore",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("user_id" = String, Path, description = "Target Matrix user id (@user:server)"),
    ),
    responses(
        (status = 200, description = "User ignored", body = ApiResponse<serde_json::Value>),
        (status = 400, description = "Malformed request, or an attempt to ignore this account's own user id", body = crate::response::ErrorResponse),
        (status = 404, description = "Account not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "account-actions",
)]
pub async fn ignore_user(
    State(account_actions): State<Arc<dyn AccountActionsSender>>,
    Path((account_id, user_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<serde_json::Value>, ApiError> {
    let started_at = Instant::now();
    match account_actions.ignore_user(account_id, &user_id).await {
        Ok(()) => {
            tracing::info!(
                account_id = %account_id,
                user_id = %user_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "account_actions: ignore_user succeeded"
            );
            Ok(ApiResponse::new(serde_json::json!({})))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                user_id = %user_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "account_actions: ignore_user failed"
            );
            Err(err.into())
        }
    }
}

/// Stop ignoring a user
///
/// Remove `user_id` from this account's ignore list.
#[utoipa::path(
    delete,
    path = "/v1/accounts/{account_id}/users/{user_id}/ignore",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("user_id" = String, Path, description = "Target Matrix user id (@user:server)"),
    ),
    responses(
        (status = 200, description = "User unignored", body = ApiResponse<serde_json::Value>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 404, description = "Account not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "account-actions",
)]
pub async fn unignore_user(
    State(account_actions): State<Arc<dyn AccountActionsSender>>,
    Path((account_id, user_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<serde_json::Value>, ApiError> {
    let started_at = Instant::now();
    match account_actions.unignore_user(account_id, &user_id).await {
        Ok(()) => {
            tracing::info!(
                account_id = %account_id,
                user_id = %user_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "account_actions: unignore_user succeeded"
            );
            Ok(ApiResponse::new(serde_json::json!({})))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                user_id = %user_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "account_actions: unignore_user failed"
            );
            Err(err.into())
        }
    }
}

/// Search the public room directory
///
/// Search a homeserver's public-room directory.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/directory/public_rooms",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        PublicRoomsQueryDto,
    ),
    responses(
        (status = 200, description = "A page of public rooms", body = ApiResponse<PublicRoomsPageDto>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 404, description = "Account not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "account-actions",
)]
pub async fn search_public_rooms(
    State(account_actions): State<Arc<dyn AccountActionsSender>>,
    Path(account_id): Path<Uuid>,
    Query(mut q): Query<PublicRoomsQueryDto>,
) -> Result<ApiResponse<PublicRoomsPageDto>, ApiError> {
    let started_at = Instant::now();
    q.limit = q.limit.map(|limit| limit.min(MAX_PUBLIC_ROOMS_LIMIT));
    match account_actions.public_rooms(account_id, q.into()).await {
        Ok(page) => {
            tracing::info!(
                account_id = %account_id,
                found = page.chunk.len(),
                elapsed_ms = started_at.elapsed().as_millis(),
                "account_actions: search_public_rooms succeeded"
            );
            Ok(ApiResponse::new(PublicRoomsPageDto::from(page)))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "account_actions: search_public_rooms failed"
            );
            Err(err.into())
        }
    }
}
