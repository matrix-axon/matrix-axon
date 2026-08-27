//! Matrix OAuth QR account-acquisition endpoints (ADR 0097).

use std::sync::Arc;

use axum::{extract::State, http::StatusCode};
use uuid::Uuid;

use crate::{
    extract::{Json, Path},
    matrix_oauth_acquire::{
        CreateMatrixOAuthQrRequest, MatrixOAuthQrAcquireService, MatrixOAuthQrFlowDto,
        SubmitMatrixOAuthCheckCodeRequest, SubmitMatrixOAuthQrRequest,
    },
    response::{ApiError, ApiResponse},
};

/// Start a QR login that creates and verifies Axon's Matrix device without a
/// Matrix password, imported access token, or recovery key.
#[utoipa::path(
    post,
    path = "/v1/accounts/login/qr",
    request_body = CreateMatrixOAuthQrRequest,
    responses(
        (status = 201, description = "QR login flow created", body = ApiResponse<MatrixOAuthQrFlowDto>),
        (status = 400, description = "Invalid Matrix user ID or presentation", body = crate::response::ErrorResponse),
        (status = 409, description = "A flow or active lifecycle operation already owns this identity", body = crate::response::ErrorResponse),
        (status = 413, description = "Request body exceeds the QR-login limit", body = crate::response::ErrorResponse),
        (status = 429, description = "Global active or retained QR flow capacity reached", body = crate::response::ErrorResponse),
    ),
    tag = "matrix-oauth",
)]
pub async fn create(
    State(service): State<Arc<dyn MatrixOAuthQrAcquireService>>,
    Json(request): Json<CreateMatrixOAuthQrRequest>,
) -> Result<(StatusCode, ApiResponse<MatrixOAuthQrFlowDto>), ApiError> {
    let flow = service
        .create(&request.expected_user_id, request.presentation)
        .await?;
    Ok((StatusCode::CREATED, ApiResponse::new(flow)))
}

/// Read the current replayable flow stage and only its stage-appropriate data.
#[utoipa::path(
    get,
    path = "/v1/accounts/login/qr/{flow_id}",
    params(("flow_id" = Uuid, Path, description = "QR login flow id")),
    responses(
        (status = 200, description = "Current QR login state", body = ApiResponse<MatrixOAuthQrFlowDto>),
        (status = 404, description = "Unknown or expired flow", body = crate::response::ErrorResponse),
    ),
    tag = "matrix-oauth",
)]
pub async fn get(
    State(service): State<Arc<dyn MatrixOAuthQrAcquireService>>,
    Path(flow_id): Path<Uuid>,
) -> Result<ApiResponse<MatrixOAuthQrFlowDto>, ApiError> {
    Ok(ApiResponse::new(service.get(flow_id).await?))
}

/// Supply one decoded QR payload to a scan-presentation flow.
#[utoipa::path(
    post,
    path = "/v1/accounts/login/qr/{flow_id}/scan",
    params(("flow_id" = Uuid, Path, description = "QR login flow id")),
    request_body = SubmitMatrixOAuthQrRequest,
    responses(
        (status = 200, description = "QR payload accepted", body = ApiResponse<MatrixOAuthQrFlowDto>),
        (status = 400, description = "Malformed, oversized, or wrong-intent QR payload", body = crate::response::ErrorResponse),
        (status = 404, description = "Unknown or expired flow", body = crate::response::ErrorResponse),
        (status = 409, description = "Wrong presentation/stage or QR input already consumed", body = crate::response::ErrorResponse),
        (status = 413, description = "Request body exceeds the QR-login limit", body = crate::response::ErrorResponse),
    ),
    tag = "matrix-oauth",
)]
pub async fn submit_scan(
    State(service): State<Arc<dyn MatrixOAuthQrAcquireService>>,
    Path(flow_id): Path<Uuid>,
    Json(request): Json<SubmitMatrixOAuthQrRequest>,
) -> Result<ApiResponse<MatrixOAuthQrFlowDto>, ApiError> {
    Ok(ApiResponse::new(
        service.submit_scan(flow_id, &request.qr_code_data).await?,
    ))
}

/// Supply one two-digit check code to a display-presentation flow.
#[utoipa::path(
    post,
    path = "/v1/accounts/login/qr/{flow_id}/check-code",
    params(("flow_id" = Uuid, Path, description = "QR login flow id")),
    request_body = SubmitMatrixOAuthCheckCodeRequest,
    responses(
        (status = 200, description = "Check code accepted", body = ApiResponse<MatrixOAuthQrFlowDto>),
        (status = 400, description = "Check code is not exactly two decimal digits", body = crate::response::ErrorResponse),
        (status = 404, description = "Unknown or expired flow", body = crate::response::ErrorResponse),
        (status = 409, description = "Wrong presentation/stage or check code already consumed", body = crate::response::ErrorResponse),
        (status = 413, description = "Request body exceeds the QR-login limit", body = crate::response::ErrorResponse),
    ),
    tag = "matrix-oauth",
)]
pub async fn submit_check_code(
    State(service): State<Arc<dyn MatrixOAuthQrAcquireService>>,
    Path(flow_id): Path<Uuid>,
    Json(request): Json<SubmitMatrixOAuthCheckCodeRequest>,
) -> Result<ApiResponse<MatrixOAuthQrFlowDto>, ApiError> {
    Ok(ApiResponse::new(
        service
            .submit_check_code(flow_id, &request.check_code)
            .await?,
    ))
}

/// Cancel a flow idempotently, including an unknown or already-expired id.
#[utoipa::path(
    delete,
    path = "/v1/accounts/login/qr/{flow_id}",
    params(("flow_id" = Uuid, Path, description = "QR login flow id")),
    responses((status = 204, description = "Flow cancelled or already absent/terminal")),
    tag = "matrix-oauth",
)]
pub async fn cancel(
    State(service): State<Arc<dyn MatrixOAuthQrAcquireService>>,
    Path(flow_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    service.cancel(flow_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
