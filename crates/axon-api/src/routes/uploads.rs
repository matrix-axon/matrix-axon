//! Staged media-upload endpoints (M15a, ADR 0059).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use uuid::Uuid;

use crate::dto::{StageMediaUploadQuery, StagedUploadDto};
use crate::extract::{Path, Query};
use crate::response::{ApiError, ApiResponse};
use crate::uploads::{StageUploadRequest, StagedUploadService};

const MAX_FILENAME_CHARS: usize = 255;
const MAX_CONTENT_TYPE_CHARS: usize = 255;

/// Stage a media upload
///
/// Stage raw upload bytes for a later room-aware media send.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/media/uploads",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        StageMediaUploadQuery,
    ),
    request_body(
        content = String,
        content_type = "application/octet-stream",
        description = "Raw media bytes streamed to Axon's durable upload staging area"
    ),
    responses(
        (status = 200, description = "Upload staged", body = ApiResponse<StagedUploadDto>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 403, description = "Account cannot accept media mutations", body = crate::response::ErrorResponse),
        (status = 404, description = "Account not found", body = crate::response::ErrorResponse),
        (status = 413, description = "Upload too large", body = crate::response::ErrorResponse),
        (status = 503, description = "Upload timed out", body = crate::response::ErrorResponse),
    ),
    tag = "media",
)]
pub async fn stage_upload(
    State(uploads): State<Arc<dyn StagedUploadService>>,
    Path(account_id): Path<Uuid>,
    Query(query): Query<StageMediaUploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<ApiResponse<StagedUploadDto>, ApiError> {
    let filename = normalize_filename(&query.filename)?;
    let content_type = sanitize_content_type(&headers)?;
    let staged = uploads
        .stage_upload(
            StageUploadRequest {
                account_id,
                kind: query.kind,
                filename,
                content_type,
            },
            Box::pin(body.into_data_stream()),
        )
        .await?;
    Ok(ApiResponse::new(StagedUploadDto {
        upload_id: staged.upload_id,
        kind: staged.kind,
        filename: staged.filename,
        content_type: staged.content_type,
        size_bytes: staged.size_bytes,
        expires_at: staged.expires_at,
    }))
}

/// Delete a staged upload
///
/// Delete an unsent staged upload.
#[utoipa::path(
    delete,
    path = "/v1/accounts/{account_id}/media/uploads/{upload_id}",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("upload_id" = Uuid, Path, description = "Server-issued upload id"),
    ),
    responses(
        (status = 200, description = "Upload deleted", body = ApiResponse<serde_json::Value>),
        (status = 404, description = "Account/upload not found", body = crate::response::ErrorResponse),
        (status = 500, description = "Internal upload cleanup failure", body = crate::response::ErrorResponse),
    ),
    tag = "media",
)]
pub async fn delete_upload(
    State(uploads): State<Arc<dyn StagedUploadService>>,
    Path((account_id, upload_id)): Path<(Uuid, Uuid)>,
) -> Result<ApiResponse<serde_json::Value>, ApiError> {
    uploads.delete_upload(account_id, upload_id).await?;
    Ok(ApiResponse::new(serde_json::json!({})))
}

fn normalize_filename(input: &str) -> Result<String, ApiError> {
    let trimmed = input.trim();
    let basename = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed).trim();
    if basename.is_empty() || basename == "." || basename == ".." {
        return Err(ApiError::bad_request("filename must not be empty"));
    }
    if basename.chars().any(|c| c == '\0' || c.is_control()) {
        return Err(ApiError::bad_request(
            "filename must not contain control characters",
        ));
    }
    if basename.chars().count() > MAX_FILENAME_CHARS {
        return Err(ApiError::bad_request(format!(
            "filename must be at most {MAX_FILENAME_CHARS} characters"
        )));
    }
    Ok(basename.to_owned())
}

fn sanitize_content_type(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let Some(value) = headers.get(axum::http::header::CONTENT_TYPE) else {
        return Ok(None);
    };
    let raw = value
        .to_str()
        .map_err(|_| ApiError::bad_request("content-type must be valid ASCII"))?
        .trim();
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.chars().count() > MAX_CONTENT_TYPE_CHARS {
        return Err(ApiError::bad_request(format!(
            "content-type must be at most {MAX_CONTENT_TYPE_CHARS} characters"
        )));
    }
    let parsed: mime::Mime = raw
        .parse()
        .map_err(|_| ApiError::bad_request("content-type must be a valid MIME type"))?;
    Ok(Some(parsed.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{normalize_filename, sanitize_content_type};
    use axum::http::{header::CONTENT_TYPE, HeaderMap, HeaderValue};

    #[test]
    fn filename_normalization_takes_basename() {
        assert_eq!(
            normalize_filename(r"C:\Users\Ada\Pictures\cat.png").unwrap(),
            "cat.png"
        );
        assert_eq!(normalize_filename("/tmp/photo.jpg").unwrap(), "photo.jpg");
        assert!(normalize_filename("../").is_err());
    }

    #[test]
    fn content_type_must_parse() {
        let mut headers = HeaderMap::new();
        assert_eq!(sanitize_content_type(&headers).unwrap(), None);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("image/png"));
        assert_eq!(
            sanitize_content_type(&headers).unwrap(),
            Some("image/png".to_owned())
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("not a mime"));
        assert!(sanitize_content_type(&headers).is_err());
    }
}
