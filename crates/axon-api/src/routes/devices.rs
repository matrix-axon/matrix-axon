//! Device-list / discovery endpoint (M16, ADR 0060).
//!
//! The picker a client reads before starting SAS verification
//! (`crate::routes::verify`) instead of requiring a target device id blind.

use std::sync::Arc;

use axum::extract::State;
use serde::Deserialize;
use utoipa::IntoParams;
use uuid::Uuid;

use crate::devices::DeviceListService;
use crate::dto::DeviceListDto;
use crate::extract::{Path, Query};
use crate::response::{ApiError, ApiResponse};

/// Query parameters for `GET /v1/accounts/{account_id}/devices`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DevicesQuery {
    /// Matrix user id to list devices for. Omit to list the account's own
    /// devices (self-verification picker); supply to list another user's
    /// (cross-user picker, ADR 0040).
    pub user_id: Option<String>,
}

/// List devices
///
/// List a Matrix user's devices — the account's own by default, or an
/// arbitrary user via `?user_id=`. Reads the SDK's local crypto-store cache
/// on every call (kept current by the account's background sync, not by a
/// homeserver call this endpoint makes itself); no device roster is
/// persisted by axon. A `user_id` axon has never tracked (no shared
/// encrypted room) returns `200` with an empty `devices` list, not an error.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/devices",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        DevicesQuery,
    ),
    responses(
        (status = 200, description = "The target user's devices (empty if axon has never tracked this user)", body = ApiResponse<DeviceListDto>),
        (status = 400, description = "user_id is not a syntactically valid Matrix user id", body = crate::response::ErrorResponse),
        (status = 404, description = "No such account", body = crate::response::ErrorResponse),
        (status = 409, description = "The account is not active (logged out or being deleted)", body = crate::response::ErrorResponse),
        (status = 502, description = "The homeserver connection itself failed (e.g. couldn't establish/restore the SDK client)", body = crate::response::ErrorResponse),
    ),
    tag = "devices",
)]
pub async fn list_devices(
    State(devices): State<Arc<dyn DeviceListService>>,
    Path(account_id): Path<Uuid>,
    Query(q): Query<DevicesQuery>,
) -> Result<ApiResponse<DeviceListDto>, ApiError> {
    let list = devices.list(account_id, q.user_id.as_deref()).await?;
    Ok(ApiResponse::new(DeviceListDto::from(list)))
}
