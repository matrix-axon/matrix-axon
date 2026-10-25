//! Opt-in homeserver preview generation and protected thumbnail recovery.
use crate::{
    dto::{GeneratePreviewRequest, GeneratedPreviewDto, PreviewThumbnailBytes},
    extract::{Json, Path},
    ApiError, ApiResponse,
};
use axum::{
    extract::State,
    http::header,
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use uuid::Uuid;

/// Generate an opt-in homeserver link preview
///
/// Explicitly discloses the URL to the selected account's homeserver.
#[utoipa::path(post, path = "/v1/accounts/{account_id}/link-previews/generate",
    params(("account_id" = Uuid, Path)), request_body = GeneratePreviewRequest,
    responses((status = 200, body = ApiResponse<GeneratedPreviewDto>, description = "Ready preview or nonfatal fallback; original link remains sendable"),
    (status = 400, body = crate::response::ErrorResponse, description = "Invalid or nonpublic HTTP(S) URL"),
    (status = 403, body = crate::response::ErrorResponse, description = "Inactive account"),
    (status = 404, body = crate::response::ErrorResponse, description = "Account not found")), tag = "media")]
pub async fn generate(
    State(store): State<axon_store::Store>,
    State(generator): State<Arc<dyn crate::PreviewGenerator>>,
    Path(account_id): Path<Uuid>,
    Json(request): Json<GeneratePreviewRequest>,
) -> Result<ApiResponse<GeneratedPreviewDto>, ApiError> {
    if !axon_core::preview_generation::eligible_url(&request.url) {
        return Err(ApiError::bad_request("preview requires a public HTTP(S) URL of at most 4096 UTF-16 units without credentials"));
    }
    let account = store
        .get_account(account_id)
        .await?
        .ok_or_else(|| ApiError::not_found("account not found"))?;
    if account.state != axon_store::AccountState::Active {
        return Err(ApiError::forbidden("account is not active"));
    }
    // Owned task finishes staging even after HTTP disconnect; unsent bytes use
    // the existing upload TTL/sweeper and never dispatch a Matrix message.
    tokio::spawn(async move { generator.generate(account_id, request.url).await })
        .await
        .map_err(|_| ApiError::internal())?
        .map(ApiResponse::new)
}

/// Retrieve a staged preview thumbnail
///
/// Bounded authenticated bytes for protected local recovery, without consuming the handle.
#[utoipa::path(get, path = "/v1/accounts/{account_id}/media/uploads/{upload_id}/preview",
    params(("account_id" = Uuid, Path), ("upload_id" = Uuid, Path)),
    responses((status = 200, content((PreviewThumbnailBytes = "image/png"), (PreviewThumbnailBytes = "image/jpeg"), (PreviewThumbnailBytes = "image/webp")), description = "Verified PNG/JPEG/WebP bytes, at most 2 MiB; Cache-Control: no-store"),
    (status = 400, body = crate::response::ErrorResponse, description = "Invalid raster thumbnail"),
    (status = 404, body = crate::response::ErrorResponse, description = "not_found: wrong account, absent, expired, consumed, sending or missing bytes; explicitly reupload retained local bytes", example = json!({"error":{"code":"not_found","message":"thumbnail not found"}})),
    (status = 413, body = crate::response::ErrorResponse, description = "payload_too_large: thumbnail exceeds 2 MiB"),
    (status = 503, body = crate::response::ErrorResponse, description = "service_unavailable: bounded thumbnail capacity unavailable")), tag = "media")]
pub async fn thumbnail(
    State(uploads): State<Arc<dyn crate::StagedUploadService>>,
    Path((account_id, upload_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, ApiError> {
    let image =
        tokio::spawn(async move { uploads.read_preview_upload(account_id, upload_id).await })
            .await
            .map_err(|_| ApiError::internal())??;
    Ok((
        [
            (header::CONTENT_TYPE, image.mimetype),
            (header::CACHE_CONTROL, "no-store".into()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
            (header::CONTENT_DISPOSITION, "attachment".into()),
        ],
        image.bytes,
    )
        .into_response())
}
