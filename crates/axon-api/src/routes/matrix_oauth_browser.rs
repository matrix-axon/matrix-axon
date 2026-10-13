//! Matrix OAuth browser sign-in endpoints (ADR 0117).

use std::sync::Arc;

use axum::{extract::State, http::StatusCode};
use uuid::Uuid;

use crate::{
    extract::{Json, Path},
    matrix_oauth_browser::{
        CreateMatrixOAuthBrowserRequest, MatrixOAuthBrowserFlowDto, MatrixOAuthBrowserService,
    },
    response::{ApiError, ApiResponse},
};

/// Start a browser sign-in for a homeserver that delegates authentication to
/// an OAuth 2.0 authorization server.
///
/// The flow yields a link and a short code. The user opens the link in any
/// browser, on any device, signs in, and approves; meanwhile the client polls
/// this flow. No second Matrix client is needed. The account that results is
/// signed in but its device is **not** verified.
#[utoipa::path(
    post,
    path = "/v1/accounts/login/browser",
    operation_id = "create_matrix_oauth_browser",
    request_body = CreateMatrixOAuthBrowserRequest,
    responses(
        (status = 201, description = "Browser sign-in flow created", body = ApiResponse<MatrixOAuthBrowserFlowDto>),
        (status = 400, description = "Invalid Matrix user ID", body = crate::response::ErrorResponse),
        (status = 409, description = "A QR login, another browser sign-in, or an active lifecycle operation already owns this identity", body = crate::response::ErrorResponse),
        (status = 413, description = "Request body exceeds the login-flow limit", body = crate::response::ErrorResponse),
        (status = 429, description = "Global active or retained login-flow capacity reached", body = crate::response::ErrorResponse),
    ),
    tag = "matrix-oauth",
)]
pub async fn create(
    State(service): State<Arc<dyn MatrixOAuthBrowserService>>,
    Json(request): Json<CreateMatrixOAuthBrowserRequest>,
) -> Result<(StatusCode, ApiResponse<MatrixOAuthBrowserFlowDto>), ApiError> {
    let flow = service.create(&request.expected_user_id).await?;
    Ok((StatusCode::CREATED, ApiResponse::new(flow)))
}

/// Read the current replayable flow stage and only its stage-appropriate data.
///
/// The link and user code are returned only in `waiting_for_authorization`.
#[utoipa::path(
    get,
    path = "/v1/accounts/login/browser/{flow_id}",
    operation_id = "get_matrix_oauth_browser",
    params(("flow_id" = Uuid, Path, description = "Browser sign-in flow id")),
    responses(
        (status = 200, description = "Current browser sign-in state", body = ApiResponse<MatrixOAuthBrowserFlowDto>),
        (status = 404, description = "Unknown or expired flow, or a QR login flow's id", body = crate::response::ErrorResponse),
    ),
    tag = "matrix-oauth",
)]
pub async fn get(
    State(service): State<Arc<dyn MatrixOAuthBrowserService>>,
    Path(flow_id): Path<Uuid>,
) -> Result<ApiResponse<MatrixOAuthBrowserFlowDto>, ApiError> {
    Ok(ApiResponse::new(service.get(flow_id).await?))
}

/// Cancel a flow idempotently, including an unknown or already-expired id.
#[utoipa::path(
    delete,
    path = "/v1/accounts/login/browser/{flow_id}",
    operation_id = "cancel_matrix_oauth_browser",
    params(("flow_id" = Uuid, Path, description = "Browser sign-in flow id")),
    responses(
        (status = 204, description = "Flow cancelled or already absent/terminal"),
        (status = 404, description = "The id belongs to a QR login flow", body = crate::response::ErrorResponse),
    ),
    tag = "matrix-oauth",
)]
pub async fn cancel(
    State(service): State<Arc<dyn MatrixOAuthBrowserService>>,
    Path(flow_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    service.cancel(flow_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
