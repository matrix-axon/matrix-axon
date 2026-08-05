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

use std::sync::Arc;

use axum::extract::State;
use uuid::Uuid;

use crate::dto::{ReadReceiptRequest, TypingRequest};
use crate::extract::{Json, Path};
use crate::response::{ApiError, ApiResponse};
use crate::sender::EphemeralSender;

/// Mark a room read: sets both the public read receipt and the private
/// fully-read marker in one homeserver call.
///
/// `event_id` states how far the client has read in the order it displays
/// events (`origin_ts`). A Matrix receipt is interpreted in the homeserver's
/// stream order instead, and the two disagree for backfilled events — so axon
/// may name a *later-arriving* event that sorts at or before `event_id`, never
/// one the client has not displayed (ADR 0089).
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
    ephemeral
        .send_read_receipt(account_id, &room_id, &req.event_id)
        .await?;
    Ok(ApiResponse::new(serde_json::json!({})))
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
    ephemeral
        .send_typing_notice(account_id, &room_id, req.typing)
        .await?;
    Ok(ApiResponse::new(serde_json::json!({})))
}
