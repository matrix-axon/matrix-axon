//! Account-scoped Matrix OAuth QR login-grant endpoints (ADR 0097).

use std::sync::Arc;

use axum::{extract::State, http::StatusCode};
use uuid::Uuid;

use crate::{
    extract::{Json, Path},
    matrix_oauth_acquire::{SubmitMatrixOAuthCheckCodeRequest, SubmitMatrixOAuthQrRequest},
    matrix_oauth_grant::{
        CreateMatrixOAuthQrGrantRequest, MatrixOAuthQrGrantFlowDto, MatrixOAuthQrGrantService,
    },
    response::{ApiError, ApiResponse},
};

#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/login-grants/qr",
    operation_id = "create_matrix_oauth_qr_grant",
    params(("account_id" = Uuid, Path, description = "Axon account id")),
    request_body = CreateMatrixOAuthQrGrantRequest,
    responses(
        (status = 201, description = "QR login-grant flow created", body = ApiResponse<MatrixOAuthQrGrantFlowDto>),
        (status = 400, description = "Invalid presentation or request shape", body = crate::response::ErrorResponse),
        (status = 404, description = "Account not found", body = crate::response::ErrorResponse),
        (status = 409, description = "Account is inactive, untrusted, unable to export secrets, or already owns a grant", body = crate::response::ErrorResponse),
        (status = 413, description = "Request body exceeds the QR-flow limit", body = crate::response::ErrorResponse),
        (status = 429, description = "Global active or retained QR grant capacity reached", body = crate::response::ErrorResponse),
        (status = 503, description = "Account lifecycle changed or the current client is temporarily unavailable", body = crate::response::ErrorResponse),
    ),
    tag = "matrix-oauth",
)]
pub async fn create(
    State(service): State<Arc<dyn MatrixOAuthQrGrantService>>,
    Path(account_id): Path<Uuid>,
    Json(request): Json<CreateMatrixOAuthQrGrantRequest>,
) -> Result<(StatusCode, ApiResponse<MatrixOAuthQrGrantFlowDto>), ApiError> {
    let flow = service.create(account_id, request.presentation).await?;
    Ok((StatusCode::CREATED, ApiResponse::new(flow)))
}

#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/login-grants/qr/{flow_id}",
    operation_id = "get_matrix_oauth_qr_grant",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("flow_id" = Uuid, Path, description = "QR login-grant flow id"),
    ),
    responses(
        (status = 200, description = "Current QR login-grant state", body = ApiResponse<MatrixOAuthQrGrantFlowDto>),
        (status = 404, description = "Unknown, expired, or differently scoped flow", body = crate::response::ErrorResponse),
    ),
    tag = "matrix-oauth",
)]
pub async fn get(
    State(service): State<Arc<dyn MatrixOAuthQrGrantService>>,
    Path((account_id, flow_id)): Path<(Uuid, Uuid)>,
) -> Result<ApiResponse<MatrixOAuthQrGrantFlowDto>, ApiError> {
    Ok(ApiResponse::new(service.get(account_id, flow_id).await?))
}

#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/login-grants/qr/{flow_id}/scan",
    operation_id = "submit_matrix_oauth_qr_grant_scan",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("flow_id" = Uuid, Path, description = "QR login-grant flow id"),
    ),
    request_body = SubmitMatrixOAuthQrRequest,
    responses(
        (status = 200, description = "QR payload accepted", body = ApiResponse<MatrixOAuthQrGrantFlowDto>),
        (status = 400, description = "Malformed, oversized, unsafe, or wrong-intent QR payload", body = crate::response::ErrorResponse),
        (status = 404, description = "Unknown, expired, or differently scoped flow", body = crate::response::ErrorResponse),
        (status = 409, description = "Wrong presentation/stage or QR input already consumed", body = crate::response::ErrorResponse),
        (status = 413, description = "Request body exceeds the QR-flow limit", body = crate::response::ErrorResponse),
    ),
    tag = "matrix-oauth",
)]
pub async fn submit_scan(
    State(service): State<Arc<dyn MatrixOAuthQrGrantService>>,
    Path((account_id, flow_id)): Path<(Uuid, Uuid)>,
    Json(request): Json<SubmitMatrixOAuthQrRequest>,
) -> Result<ApiResponse<MatrixOAuthQrGrantFlowDto>, ApiError> {
    Ok(ApiResponse::new(
        service
            .submit_scan(account_id, flow_id, &request.qr_code_data)
            .await?,
    ))
}

#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/login-grants/qr/{flow_id}/check-code",
    operation_id = "submit_matrix_oauth_qr_grant_check_code",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("flow_id" = Uuid, Path, description = "QR login-grant flow id"),
    ),
    request_body = SubmitMatrixOAuthCheckCodeRequest,
    responses(
        (status = 200, description = "Check code accepted", body = ApiResponse<MatrixOAuthQrGrantFlowDto>),
        (status = 400, description = "Check code is not exactly two decimal digits", body = crate::response::ErrorResponse),
        (status = 404, description = "Unknown, expired, or differently scoped flow", body = crate::response::ErrorResponse),
        (status = 409, description = "Wrong presentation/stage or check code already consumed", body = crate::response::ErrorResponse),
        (status = 413, description = "Request body exceeds the QR-flow limit", body = crate::response::ErrorResponse),
    ),
    tag = "matrix-oauth",
)]
pub async fn submit_check_code(
    State(service): State<Arc<dyn MatrixOAuthQrGrantService>>,
    Path((account_id, flow_id)): Path<(Uuid, Uuid)>,
    Json(request): Json<SubmitMatrixOAuthCheckCodeRequest>,
) -> Result<ApiResponse<MatrixOAuthQrGrantFlowDto>, ApiError> {
    Ok(ApiResponse::new(
        service
            .submit_check_code(account_id, flow_id, &request.check_code)
            .await?,
    ))
}

#[utoipa::path(
    delete,
    path = "/v1/accounts/{account_id}/login-grants/qr/{flow_id}",
    operation_id = "cancel_matrix_oauth_qr_grant",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("flow_id" = Uuid, Path, description = "QR login-grant flow id"),
    ),
    responses((status = 204, description = "Flow cancelled or already absent/terminal")),
    tag = "matrix-oauth",
)]
pub async fn cancel(
    State(service): State<Arc<dyn MatrixOAuthQrGrantService>>,
    Path((account_id, flow_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    service.cancel(account_id, flow_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
