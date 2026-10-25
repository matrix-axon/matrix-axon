//! Power-level read/write (ADR 0068, M19e): role thresholds and per-user
//! levels, merged into one `m.room.power_levels` state event on write.
//!
//! Split from [`room_settings`](crate::routes::room_settings) rather than
//! folded into it because power levels are the one M19 verb where a client
//! mistake can permanently strand the caller — dropping their own resolved
//! level below what's needed to send another `m.room.power_levels` event
//! succeeds at the protocol level, with no `M_FORBIDDEN` for the usual
//! error-mapping path to catch. [`PowerLevelsSender::set_power_levels`]
//! rejects that outcome itself (mapped here to `400`) unless the request
//! sets `acknowledge_self_demotion`. Every call is logged with `account_id`,
//! `room_id`, elapsed time, and — on failure — the mapped error, matching
//! `room_settings`'s shape.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use uuid::Uuid;

use crate::dto::{PowerLevelChangesRequest, PowerLevelsDto};
use crate::extract::{Json, Path};
use crate::response::{ApiError, ApiResponse};
use crate::sender::PowerLevelsSender;

/// Set power levels
///
/// Apply a power-level change: role thresholds and/or per-user levels,
/// merged into one `m.room.power_levels` write.
#[utoipa::path(
    put,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/power_levels",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    request_body = PowerLevelChangesRequest,
    responses(
        (status = 200, description = "Power levels set", body = ApiResponse<serde_json::Value>),
        (status = 400, description = "Malformed request, or the change would strand the caller below the level needed to self-correct (set acknowledge_self_demotion to proceed anyway)", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 404, description = "Account or room not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "power-levels",
)]
pub async fn set_power_levels(
    State(power_levels): State<Arc<dyn PowerLevelsSender>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
    Json(req): Json<PowerLevelChangesRequest>,
) -> Result<ApiResponse<serde_json::Value>, ApiError> {
    let started_at = Instant::now();
    match power_levels
        .set_power_levels(account_id, &room_id, req.into())
        .await
    {
        Ok(()) => {
            tracing::info!(
                account_id = %account_id,
                room_id = %room_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "power_levels: set_power_levels succeeded"
            );
            Ok(ApiResponse::new(serde_json::json!({})))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                room_id = %room_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "power_levels: set_power_levels failed"
            );
            Err(err.into())
        }
    }
}

/// Get power levels
///
/// Read this room's fully resolved power levels (defaults filled in).
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/power_levels",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    responses(
        (status = 200, description = "Resolved power levels", body = ApiResponse<PowerLevelsDto>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 404, description = "Account or room not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "power-levels",
)]
pub async fn get_power_levels(
    State(power_levels): State<Arc<dyn PowerLevelsSender>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<PowerLevelsDto>, ApiError> {
    let started_at = Instant::now();
    match power_levels.power_levels(account_id, &room_id).await {
        Ok(resolved) => {
            tracing::info!(
                account_id = %account_id,
                room_id = %room_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                "power_levels: get_power_levels succeeded"
            );
            Ok(ApiResponse::new(resolved.into()))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                room_id = %room_id,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = ?err,
                "power_levels: get_power_levels failed"
            );
            Err(err.into())
        }
    }
}
