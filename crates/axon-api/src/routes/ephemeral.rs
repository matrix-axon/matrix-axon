//! Ephemeral, homeserver-facing outbound signals: read receipts (ADR 0067) and
//! typing notices (ADR 0068, M19a).
//!
//! Both route through the [`EphemeralSender`] port (injected via `AppState`),
//! separate from [`MessageSender`](crate::sender::MessageSender) because
//! callers treat them as best-effort — a client fire-and-forgets these from its
//! existing read-marker/typing debounce and never surfaces a failure as a
//! user-facing error. Server-side, failures still map to real HTTP statuses
//! like any other mutation; it's the client's choice not to await or surface
//! them that makes the feature best-effort, not the server hiding them.
//!
//! Precisely *because* no client surfaces these failures, both handlers log a
//! structured success/failure line — the same shape ADR 0068's M19b+ routes
//! use. A read receipt that never reaches the homeserver is otherwise
//! completely silent, and it presents as a room that will not stop showing
//! unread: the count is derived from receipt state (ADR 0070), so a failed send
//! and a successful one are indistinguishable from the outside.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use uuid::Uuid;

use crate::dto::{ReadReceiptRequest, TypingRequest};
use crate::extract::{Json, Path};
use crate::response::{ApiError, ApiResponse};
use crate::sender::EphemeralSender;

/// Mark a room read: sets both the public read receipt and the private
/// fully-read marker in one homeserver call.
///
/// `event_id` is sent to the homeserver **verbatim**. A receipt is interpreted
/// in arrival order while every axon read surface sorts by `origin_ts`, and the
/// two disagree for backfilled events — so the client must name the event with
/// the greatest `arrival_order` among those it has displayed (ADR 0089). Only
/// the client knows what it displayed, so only the client can choose it: this
/// route does not resolve, substitute, or second-guess the id it is given.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/read",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    request_body = ReadReceiptRequest,
    responses(
        (status = 200, description = "Receipt sent", body = ApiResponse<serde_json::Value>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "ephemeral",
)]
pub async fn send_read_receipt(
    State(ephemeral): State<Arc<dyn EphemeralSender>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
    Json(req): Json<ReadReceiptRequest>,
) -> Result<ApiResponse<serde_json::Value>, ApiError> {
    let started_at = Instant::now();
    match ephemeral
        .send_read_receipt(account_id, &room_id, &req.event_id)
        .await
    {
        Ok(()) => {
            tracing::debug!(
                account_id = %account_id,
                room_id = %room_id,
                event_id = %req.event_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "ephemeral: read receipt sent"
            );
            Ok(ApiResponse::new(serde_json::json!({})))
        }
        Err(err) => {
            // `warn`, not `debug`: this is the failure that reads to a user as
            // "this room will not stop showing unread", and no client will ever
            // report it.
            tracing::warn!(
                account_id = %account_id,
                room_id = %room_id,
                event_id = %req.event_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "ephemeral: read receipt failed"
            );
            Err(err.into())
        }
    }
}

/// Set (or clear) this account's typing indicator in a room.
#[utoipa::path(
    put,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/typing",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    request_body = TypingRequest,
    responses(
        (status = 200, description = "Typing notice sent", body = ApiResponse<serde_json::Value>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "ephemeral",
)]
pub async fn send_typing_notice(
    State(ephemeral): State<Arc<dyn EphemeralSender>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
    Json(req): Json<TypingRequest>,
) -> Result<ApiResponse<serde_json::Value>, ApiError> {
    let started_at = Instant::now();
    match ephemeral
        .send_typing_notice(account_id, &room_id, req.typing)
        .await
    {
        Ok(()) => {
            tracing::debug!(
                account_id = %account_id,
                room_id = %room_id,
                typing = req.typing,
                elapsed_ms = started_at.elapsed().as_millis(),
                "ephemeral: typing notice sent"
            );
            Ok(ApiResponse::new(serde_json::json!({})))
        }
        Err(err) => {
            // Only `debug`: unlike a read receipt, a dropped typing notice
            // leaves no durable wrong state behind — it expires on its own.
            tracing::debug!(
                account_id = %account_id,
                room_id = %room_id,
                typing = req.typing,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "ephemeral: typing notice failed"
            );
            Err(err.into())
        }
    }
}
