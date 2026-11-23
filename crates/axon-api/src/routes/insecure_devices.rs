//! Account-scoped MSC4153 policy and preflight.

use std::sync::Arc;

use axon_core::insecure_devices::InsecureDeviceImpact;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::extract::{Json, Path};
use crate::lifecycle::{AccountLifecycle, InsecureDevicesError};
use crate::response::{ApiError, ApiResponse, ErrorBody, ErrorResponse};

#[derive(Debug, Serialize, ToSchema)]
pub struct InsecureDeviceRoomDto {
    pub room_id: String,
    /// Members with at least one known device that is not cross-signed.
    pub member_user_ids: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InsecureDeviceImpactDto {
    pub rooms: Vec<InsecureDeviceRoomDto>,
    /// Pass this value back with the enable request, even if the list is empty.
    pub acknowledgement_token: String,
}

impl From<InsecureDeviceImpact> for InsecureDeviceImpactDto {
    fn from(impact: InsecureDeviceImpact) -> Self {
        Self {
            rooms: impact
                .rooms
                .into_iter()
                .map(|room| InsecureDeviceRoomDto {
                    room_id: room.room_id,
                    member_user_ids: room.member_user_ids,
                })
                .collect(),
            acknowledgement_token: impact.acknowledgement_token,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SetInsecureDevicesRequest {
    pub enabled: bool,
    /// Required when switching from off to on. Use the current impact token.
    pub acknowledgement_token: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InsecureDevicesSettingDto {
    pub enabled: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct InsecureDevicesConflictResponse {
    error: ErrorBody,
    /// Present when the acknowledgement was missing or stale.
    #[serde(skip_serializing_if = "Option::is_none")]
    impact: Option<InsecureDeviceImpactDto>,
}

fn conflict(code: &str, message: &str, impact: Option<InsecureDeviceImpact>) -> Response {
    let body = InsecureDevicesConflictResponse {
        error: ErrorBody {
            code: code.to_owned(),
            message: message.to_owned(),
        },
        impact: impact.map(Into::into),
    };
    (StatusCode::CONFLICT, axum::Json(body)).into_response()
}

fn error_response(error: InsecureDevicesError) -> Response {
    match error {
        InsecureDevicesError::NotFound(message) => ApiError::not_found(message).into_response(),
        InsecureDevicesError::Conflict(message) => ApiError::conflict(message).into_response(),
        InsecureDevicesError::OwnDeviceNotCrossSigned => conflict(
            "own_device_not_cross_signed",
            "Verify or recover Axon's device before enabling this setting.",
            None,
        ),
        InsecureDevicesError::OwnIdentityNotVerified => conflict(
            "own_identity_not_verified",
            "Verify Axon's own cross-signing identity or recover its cross-signing keys before enabling this setting.",
            None,
        ),
        InsecureDevicesError::OwnTrustUnavailable => ApiError::service_unavailable(
            "Axon's device trust could not be read. Retry shortly.",
        )
        .into_response(),
        InsecureDevicesError::AcknowledgementRequired(impact) => conflict(
            "impact_acknowledgement_required",
            "Review and acknowledge the current impact before enabling this setting.",
            Some(impact),
        ),
        InsecureDevicesError::Unavailable => ApiError::service_unavailable(
            "The account or its room rosters are not ready. Retry after it has synced.",
        )
        .into_response(),
        InsecureDevicesError::Internal => ApiError::internal().into_response(),
    }
}

/// Read the impact of insecure device exclusion
///
/// Lists joined encrypted rooms and the members with affected devices known to
/// Axon. Devices that have not uploaded identity keys are absent from the list.
/// The server confirms rooms whose cache says they are unencrypted and may
/// sync lazy rosters before this read succeeds.
#[utoipa::path(
    get,
    operation_id = "insecure_devices_impact",
    path = "/v1/accounts/{account_id}/exclude_insecure_devices/impact",
    params(("account_id" = Uuid, Path, description = "Axon account id")),
    responses(
        (status = 200, description = "Current impact and acknowledgement token", body = ApiResponse<InsecureDeviceImpactDto>),
        (status = 404, description = "Account not found", body = ErrorResponse),
        (status = 409, description = "Account is inactive or being deleted", body = ErrorResponse),
        (status = 500, description = "Internal error", body = ErrorResponse),
        (status = 503, description = "The account connection, room encryption state, room rosters, or local crypto store are unavailable", body = ErrorResponse),
    ),
    tag = "accounts",
)]
pub async fn impact(
    State(lifecycle): State<Arc<dyn AccountLifecycle>>,
    Path(account_id): Path<Uuid>,
) -> Result<ApiResponse<InsecureDeviceImpactDto>, Response> {
    let impact = lifecycle
        .insecure_devices_impact(account_id)
        .await
        .map_err(error_response)?;
    Ok(ApiResponse::new(impact.into()))
}

/// Set insecure device exclusion
///
/// Enabling requires a cross-signed Axon device, a locally verified own
/// cross-signing identity, and acknowledgement of the current impact. Disabling needs only a valid bearer. A changed
/// setting on an active account restarts its SDK session with the persisted
/// policy. Previously decrypted history is preserved.
#[utoipa::path(
    put,
    operation_id = "set_exclude_insecure_devices",
    path = "/v1/accounts/{account_id}/exclude_insecure_devices",
    params(("account_id" = Uuid, Path, description = "Axon account id")),
    request_body = SetInsecureDevicesRequest,
    responses(
        (status = 200, description = "The persisted setting", body = ApiResponse<InsecureDevicesSettingDto>),
        (status = 400, description = "Malformed request", body = ErrorResponse),
        (status = 404, description = "Account not found", body = ErrorResponse),
        (status = 409, description = "Account busy or in a conflicting state, own device not cross-signed, own identity not verified, or missing or stale acknowledgement. For impact_acknowledgement_required, review the returned impact before retrying.", body = InsecureDevicesConflictResponse),
        (status = 500, description = "Internal error", body = ErrorResponse),
        (status = 503, description = "The account connection, room encryption state, room rosters, or local crypto store are unavailable", body = ErrorResponse),
    ),
    tag = "accounts",
)]
pub async fn set(
    State(lifecycle): State<Arc<dyn AccountLifecycle>>,
    Path(account_id): Path<Uuid>,
    Json(request): Json<SetInsecureDevicesRequest>,
) -> Result<ApiResponse<InsecureDevicesSettingDto>, Response> {
    lifecycle
        .set_exclude_insecure_devices(
            account_id,
            request.enabled,
            request.acknowledgement_token.as_deref(),
        )
        .await
        .map_err(error_response)?;
    Ok(ApiResponse::new(InsecureDevicesSettingDto {
        enabled: request.enabled,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_acknowledgement_returns_the_current_impact() {
        let response = error_response(InsecureDevicesError::AcknowledgementRequired(
            InsecureDeviceImpact {
                rooms: vec![axon_core::insecure_devices::InsecureDeviceRoom {
                    room_id: "!room:example.org".to_owned(),
                    member_user_ids: vec!["@bridge:example.org".to_owned()],
                }],
                acknowledgement_token: "current".to_owned(),
            },
        ));
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["code"], "impact_acknowledgement_required");
        assert_eq!(
            json["impact"],
            serde_json::json!({
                "rooms": [{
                    "room_id": "!room:example.org",
                    "member_user_ids": ["@bridge:example.org"],
                }],
                "acknowledgement_token": "current",
            })
        );
    }
}
