//! Existing-room membership mutations (ADR 0068, M19b): `leave`, `forget`,
//! `invite`, `kick`, `ban`, `unban`.
//!
//! Each handler routes through the [`MembershipSender`] port (injected via
//! `AppState`), mirroring [`messages`](crate::routes::messages) — all six
//! except `leave` resolve through the same `SdkGateway::room()` path as
//! `send_message`/`redact`. `leave` also declines a pending invite whose
//! local SDK `Room` has been evicted (ADR 0091), so it must not require
//! `get_room`. None return an event id: the resulting `m.room.member` state
//! event round-trips through sync like any other state change, so the
//! success envelope carries an empty object, same as
//! [`ephemeral`](crate::routes::ephemeral). Every call is logged (ADR 0068's
//! structured-logging requirement for M19 routes) with `account_id`,
//! `room_id`, the target user where applicable, elapsed time, and — on
//! failure — the mapped error, since a moderation action (kick/ban/unban)
//! failing silently is hard to diagnose after the fact.

use std::sync::Arc;
use std::time::Instant;

use axon_core::{InviteRemovedFrame, LiveFrame};
use axon_store::Store;
use axum::extract::State;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::dto::{InviteRequest, MemberActionRequest};
use crate::extract::{Json, Path};
use crate::response::{ApiError, ApiResponse};
use crate::sender::{LeaveOutcome, MembershipSender};

/// Leave this room.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/leave",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    responses(
        (status = 200, description = "Left the room", body = ApiResponse<serde_json::Value>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 404, description = "Account or room not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "membership",
)]
pub async fn leave_room(
    State(membership): State<Arc<dyn MembershipSender>>,
    State(store): State<Store>,
    State(live): State<broadcast::Sender<LiveFrame>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<serde_json::Value>, ApiError> {
    let started_at = Instant::now();
    match membership.leave(account_id, &room_id).await {
        Ok(outcome) => {
            // A declined invite whose Room the SDK already evicted will never
            // hit ADR 0091's positive-absence prune (`get_room` stays None),
            // so drop the row here — otherwise reject would resurrect the
            // invite on the next GET /v1/invites.
            //
            // Only on a confirmed leave. `Unconfirmed` is not evidence the
            // membership is gone; deleting then would silently destroy an
            // invite that is still pending. A successful decline via
            // `Room::leave` is `Left` too, so this cannot be gated on "looks
            // like an invite" — every ordinary room leave issues one
            // indexed DELETE that matches nothing. That miss is accepted
            // (same shape as `note_room_reachability`'s clear); a SELECT
            // first would cost more than the no-op delete.
            if outcome == LeaveOutcome::Left {
                match store.delete_room_invite(account_id, &room_id).await {
                    Ok(true) => {
                        let _ = live.send(LiveFrame::InviteRemoved(InviteRemovedFrame {
                            account_id,
                            room_id: room_id.clone(),
                        }));
                    }
                    Ok(false) => {}
                    Err(err) => {
                        tracing::warn!(
                            account_id = %account_id,
                            room_id = %room_id,
                            error = %err,
                            "membership: leave succeeded but failed to drop pending invite"
                        );
                    }
                }
            }
            tracing::info!(
                account_id = %account_id,
                room_id = %room_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                confirmed = outcome == LeaveOutcome::Left,
                "membership: leave succeeded"
            );
            Ok(ApiResponse::new(serde_json::json!({})))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                room_id = %room_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "membership: leave failed"
            );
            Err(err.into())
        }
    }
}

/// Forget a left or banned-from room.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/forget",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    responses(
        (status = 200, description = "Room forgotten", body = ApiResponse<serde_json::Value>),
        (status = 400, description = "Malformed request, or the room isn't left/banned", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 404, description = "Account or room not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "membership",
)]
pub async fn forget_room(
    State(membership): State<Arc<dyn MembershipSender>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<serde_json::Value>, ApiError> {
    let started_at = Instant::now();
    match membership.forget(account_id, &room_id).await {
        Ok(()) => {
            tracing::info!(
                account_id = %account_id,
                room_id = %room_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "membership: forget succeeded"
            );
            Ok(ApiResponse::new(serde_json::json!({})))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                room_id = %room_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "membership: forget failed"
            );
            Err(err.into())
        }
    }
}

/// Invite a user to this room.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/invite",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    request_body = InviteRequest,
    responses(
        (status = 200, description = "Invite sent", body = ApiResponse<serde_json::Value>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 404, description = "Account or room not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "membership",
)]
pub async fn invite_user(
    State(membership): State<Arc<dyn MembershipSender>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
    Json(req): Json<InviteRequest>,
) -> Result<ApiResponse<serde_json::Value>, ApiError> {
    let started_at = Instant::now();
    match membership.invite(account_id, &room_id, &req.user_id).await {
        Ok(()) => {
            tracing::info!(
                account_id = %account_id,
                room_id = %room_id,
                target_user_id = %req.user_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "membership: invite succeeded"
            );
            Ok(ApiResponse::new(serde_json::json!({})))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                room_id = %room_id,
                target_user_id = %req.user_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "membership: invite failed"
            );
            Err(err.into())
        }
    }
}

/// Kick a user from this room, optionally with a reason.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/kick",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    request_body = MemberActionRequest,
    responses(
        (status = 200, description = "User kicked", body = ApiResponse<serde_json::Value>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 404, description = "Account or room not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "membership",
)]
pub async fn kick_user(
    State(membership): State<Arc<dyn MembershipSender>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
    Json(req): Json<MemberActionRequest>,
) -> Result<ApiResponse<serde_json::Value>, ApiError> {
    let started_at = Instant::now();
    match membership
        .kick(account_id, &room_id, &req.user_id, req.reason.as_deref())
        .await
    {
        Ok(()) => {
            tracing::info!(
                account_id = %account_id,
                room_id = %room_id,
                target_user_id = %req.user_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "membership: kick succeeded"
            );
            Ok(ApiResponse::new(serde_json::json!({})))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                room_id = %room_id,
                target_user_id = %req.user_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "membership: kick failed"
            );
            Err(err.into())
        }
    }
}

/// Ban a user from this room, optionally with a reason.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/ban",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    request_body = MemberActionRequest,
    responses(
        (status = 200, description = "User banned", body = ApiResponse<serde_json::Value>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 404, description = "Account or room not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "membership",
)]
pub async fn ban_user(
    State(membership): State<Arc<dyn MembershipSender>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
    Json(req): Json<MemberActionRequest>,
) -> Result<ApiResponse<serde_json::Value>, ApiError> {
    let started_at = Instant::now();
    match membership
        .ban(account_id, &room_id, &req.user_id, req.reason.as_deref())
        .await
    {
        Ok(()) => {
            tracing::info!(
                account_id = %account_id,
                room_id = %room_id,
                target_user_id = %req.user_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "membership: ban succeeded"
            );
            Ok(ApiResponse::new(serde_json::json!({})))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                room_id = %room_id,
                target_user_id = %req.user_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "membership: ban failed"
            );
            Err(err.into())
        }
    }
}

/// Unban a user from this room, optionally with a reason.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/unban",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    request_body = MemberActionRequest,
    responses(
        (status = 200, description = "User unbanned", body = ApiResponse<serde_json::Value>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 404, description = "Account or room not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "membership",
)]
pub async fn unban_user(
    State(membership): State<Arc<dyn MembershipSender>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
    Json(req): Json<MemberActionRequest>,
) -> Result<ApiResponse<serde_json::Value>, ApiError> {
    let started_at = Instant::now();
    match membership
        .unban(account_id, &room_id, &req.user_id, req.reason.as_deref())
        .await
    {
        Ok(()) => {
            tracing::info!(
                account_id = %account_id,
                room_id = %room_id,
                target_user_id = %req.user_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "membership: unban succeeded"
            );
            Ok(ApiResponse::new(serde_json::json!({})))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                room_id = %room_id,
                target_user_id = %req.user_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "membership: unban failed"
            );
            Err(err.into())
        }
    }
}
