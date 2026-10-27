//! Room-entry mutations (ADR 0068, M19c): `join`, `knock`, `create_room`,
//! `create_dm`.
//!
//! Unlike [`membership`](crate::routes::membership) (M19b), none of these
//! resolve through an existing `Room` — there is no room yet — so they go
//! through the [`RoomEntrySender`] port straight to
//! `ClientManager::get_or_connect`. Every call returns the resulting room's
//! id, so (unlike membership) the success envelope carries
//! [`RoomEntryResultDto`], not an empty object. Each handler is logged (ADR
//! 0068's structured-logging requirement for M19 routes) with `account_id`,
//! the resolved/target room or user, elapsed time, and — on failure — the
//! mapped error.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use uuid::Uuid;

use crate::dto::{
    CreateDmRequest, CreateRoomRequestDto, JoinRoomRequest, KnockRoomRequest, RoomEntryResultDto,
};
use crate::extract::{Json, Path};
use crate::response::{ApiError, ApiResponse};
use crate::sender::RoomEntrySender;

/// Join a room
///
/// Join a room by id or alias.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/rooms/join",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
    ),
    request_body = JoinRoomRequest,
    responses(
        (status = 200, description = "Joined the room", body = ApiResponse<RoomEntryResultDto>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 404, description = "Account not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 504, description = "The homeserver did not answer in time (ADR 0121); a write may still take effect", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "room-entry",
)]
pub async fn join_room(
    State(room_entry): State<Arc<dyn RoomEntrySender>>,
    Path(account_id): Path<Uuid>,
    Json(req): Json<JoinRoomRequest>,
) -> Result<ApiResponse<RoomEntryResultDto>, ApiError> {
    let started_at = Instant::now();
    match room_entry
        .join(account_id, &req.room_id_or_alias, &req.server_names)
        .await
    {
        Ok(room_id) => {
            tracing::info!(
                account_id = %account_id,
                room_id = %room_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "room entry: join succeeded"
            );
            Ok(ApiResponse::new(RoomEntryResultDto { room_id }))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                target = %req.room_id_or_alias,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "room entry: join failed"
            );
            Err(err.into())
        }
    }
}

/// Knock on a room
///
/// Knock on a room, optionally with a reason.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/rooms/knock",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
    ),
    request_body = KnockRoomRequest,
    responses(
        (status = 200, description = "Knocked on the room", body = ApiResponse<RoomEntryResultDto>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted (e.g. the room doesn't allow knocking)", body = crate::response::ErrorResponse),
        (status = 404, description = "Account not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 504, description = "The homeserver did not answer in time (ADR 0121); a write may still take effect", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "room-entry",
)]
pub async fn knock_room(
    State(room_entry): State<Arc<dyn RoomEntrySender>>,
    Path(account_id): Path<Uuid>,
    Json(req): Json<KnockRoomRequest>,
) -> Result<ApiResponse<RoomEntryResultDto>, ApiError> {
    let started_at = Instant::now();
    match room_entry
        .knock(
            account_id,
            &req.room_id_or_alias,
            req.reason.as_deref(),
            &req.server_names,
        )
        .await
    {
        Ok(room_id) => {
            tracing::info!(
                account_id = %account_id,
                room_id = %room_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "room entry: knock succeeded"
            );
            Ok(ApiResponse::new(RoomEntryResultDto { room_id }))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                target = %req.room_id_or_alias,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "room entry: knock failed"
            );
            Err(err.into())
        }
    }
}

/// Create a direct message
///
/// Create a DM with another user.
///
/// **Not idempotent**: Matrix's `createRoom` endpoint has no idempotency key,
/// so retrying this call after a client-side timeout can create a second DM
/// room upstream if the first attempt actually succeeded after Axon's own
/// timeout fired (see `SdkGateway::create_dm`). This route always creates a
/// new room; it does not check for or reuse an existing DM with the target
/// user.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/rooms/dm",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
    ),
    request_body = CreateDmRequest,
    responses(
        (status = 200, description = "DM room created", body = ApiResponse<RoomEntryResultDto>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 404, description = "Account not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 504, description = "The homeserver did not answer in time (ADR 0121); a write may still take effect", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "room-entry",
)]
pub async fn create_dm(
    State(room_entry): State<Arc<dyn RoomEntrySender>>,
    Path(account_id): Path<Uuid>,
    Json(req): Json<CreateDmRequest>,
) -> Result<ApiResponse<RoomEntryResultDto>, ApiError> {
    let started_at = Instant::now();
    match room_entry.create_dm(account_id, &req.user_id).await {
        Ok(room_id) => {
            tracing::info!(
                account_id = %account_id,
                room_id = %room_id,
                target_user_id = %req.user_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "room entry: create_dm succeeded"
            );
            Ok(ApiResponse::new(RoomEntryResultDto { room_id }))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                target_user_id = %req.user_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "room entry: create_dm failed"
            );
            Err(err.into())
        }
    }
}

/// Create a room
///
/// Create a new room.
///
/// **Not idempotent**: Matrix's `createRoom` endpoint has no idempotency key,
/// so retrying this call after a client-side timeout can create a second
/// room upstream if the first attempt actually succeeded after Axon's own
/// timeout fired (see `SdkGateway::create_room`).
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/rooms",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
    ),
    request_body = CreateRoomRequestDto,
    responses(
        (status = 200, description = "Room created", body = ApiResponse<RoomEntryResultDto>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 404, description = "Account not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 504, description = "The homeserver did not answer in time (ADR 0121); a write may still take effect", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "room-entry",
)]
pub async fn create_room(
    State(room_entry): State<Arc<dyn RoomEntrySender>>,
    Path(account_id): Path<Uuid>,
    Json(req): Json<CreateRoomRequestDto>,
) -> Result<ApiResponse<RoomEntryResultDto>, ApiError> {
    let started_at = Instant::now();
    match room_entry.create_room(account_id, req.into()).await {
        Ok(room_id) => {
            tracing::info!(
                account_id = %account_id,
                room_id = %room_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "room entry: create_room succeeded"
            );
            Ok(ApiResponse::new(RoomEntryResultDto { room_id }))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "room entry: create_room failed"
            );
            Err(err.into())
        }
    }
}
