//! Port for staged media uploads (M15a, ADR 0059).
//!
//! The API crate owns the capability it needs: accept a bounded stream of bytes,
//! persist normalized metadata, and later delete an unsent staged upload. The
//! concrete filesystem + store implementation is wired in by `axon-server`.

use std::future::Future;
use std::pin::Pin;

use async_trait::async_trait;
use axon_core::{MediaAttachment, MediaSendKind};
use bytes::Bytes;
use futures_util::Stream;
use uuid::Uuid;

use crate::dto::MediaUploadKindDto;
use crate::response::ApiError;
use crate::sender::SendError;

/// Raw upload request body as a fallible byte stream.
pub type UploadStream = Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send + 'static>>;

/// Metadata needed to stage one media upload.
#[derive(Debug, Clone)]
pub struct StageUploadRequest {
    pub account_id: Uuid,
    pub kind: MediaUploadKindDto,
    pub filename: String,
    pub content_type: Option<String>,
}

/// Normalized metadata returned after a successful stage operation.
#[derive(Debug, Clone)]
pub struct StagedUpload {
    pub upload_id: Uuid,
    pub kind: MediaUploadKindDto,
    pub filename: String,
    pub content_type: Option<String>,
    pub size_bytes: u64,
    pub expires_at: String,
}

/// Bytes and metadata for a staged upload that has been claimed for sending.
#[derive(Debug, Clone)]
pub struct ClaimedUpload {
    pub upload_id: Uuid,
    pub kind: MediaUploadKindDto,
    pub filename: String,
    pub content_type: Option<String>,
    pub size_bytes: u64,
    pub bytes: Vec<u8>,
}

/// Every consumer of a claimed upload (`send_media`, `set_room_avatar`, and
/// future M19f profile-avatar routes) needs the same bytes handed to the
/// outbound-message/room-settings gateway as a [`MediaAttachment`]; sharing
/// this conversion keeps that mapping in one place instead of re-deriving it
/// per handler.
impl From<ClaimedUpload> for MediaAttachment {
    fn from(upload: ClaimedUpload) -> Self {
        MediaAttachment {
            kind: match upload.kind {
                MediaUploadKindDto::Image => MediaSendKind::Image,
                MediaUploadKindDto::File => MediaSendKind::File,
            },
            filename: upload.filename,
            content_type: upload.content_type,
            size_bytes: upload.size_bytes,
            bytes: upload.bytes,
        }
    }
}

/// What can go wrong while staging or deleting upload bytes.
#[derive(Debug)]
pub enum StageUploadError {
    /// Malformed request metadata or body stream. -> `400`.
    Invalid(String),
    /// The account or upload does not exist. -> `404`.
    NotFound(String),
    /// The account exists but cannot accept mutations. -> `403`.
    Forbidden(String),
    /// The body exceeded the configured cap. -> `413`.
    TooLarge { cap: u64 },
    /// The client upload timed out. -> `503`.
    Timeout(String),
    /// Local filesystem/database failure. Logged and returned as a generic `500`.
    Internal(String),
}

/// Stages and deletes client-originated upload bytes.
#[async_trait]
pub trait StagedUploadService: Send + Sync {
    /// Read a verified thumbnail without claiming/consuming it. Wrong-account,
    /// expired, sending, consumed and missing bytes all return NotFound.
    async fn read_preview_upload(
        &self,
        _account_id: Uuid,
        _upload_id: Uuid,
    ) -> Result<axon_core::link_preview::PreviewImage, StageUploadError> {
        Err(StageUploadError::NotFound("thumbnail not found".into()))
    }

    /// Claim a thumbnail with a 2 MiB read cap and verified raster metadata.
    /// Implementations must reject before allocating from the ordinary upload
    /// size. On validation/read failure, release their own acquired claim.
    async fn claim_preview_upload(
        &self,
        _account_id: Uuid,
        _upload_id: Uuid,
    ) -> Result<axon_core::link_preview::PreviewImage, StageUploadError> {
        Err(StageUploadError::Invalid(
            "preview images unavailable".into(),
        ))
    }
    async fn stage_upload(
        &self,
        request: StageUploadRequest,
        body: UploadStream,
    ) -> Result<StagedUpload, StageUploadError>;

    async fn delete_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<(), StageUploadError>;

    async fn claim_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<ClaimedUpload, StageUploadError>;

    async fn complete_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<(), StageUploadError>;

    async fn release_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<(), StageUploadError>;
}

/// Claims `upload_id`, runs `mutate` against the resulting attachment, then
/// completes the staged upload on success or releases it on failure — the
/// claim → mutate → complete-or-release control flow shared by every route
/// that consumes a staged upload (`send_media`, room avatar set, account
/// avatar set). A failed claim propagates immediately (nothing was claimed
/// yet to release); a failed complete/release afterward is logged (`verb`
/// names the mutation in the log line) but never overrides the caller's own
/// success/failure result — completion/release are best-effort bookkeeping,
/// not fatal to the request, mirroring every existing staged-upload route's
/// established behavior.
pub(crate) async fn with_staged_upload<T, F, Fut>(
    uploads: &dyn StagedUploadService,
    account_id: Uuid,
    upload_id: Uuid,
    verb: &str,
    mutate: F,
) -> Result<T, ApiError>
where
    F: FnOnce(MediaAttachment) -> Fut,
    Fut: Future<Output = Result<T, SendError>>,
{
    let upload = uploads.claim_upload(account_id, upload_id).await?;
    let attachment: MediaAttachment = upload.into();
    match mutate(attachment).await {
        Ok(value) => {
            if let Err(err) = uploads.complete_upload(account_id, upload_id).await {
                tracing::warn!(
                    account_id = %account_id,
                    upload_id = %upload_id,
                    error = ?err,
                    "{verb} but failed to consume staged upload"
                );
            }
            Ok(value)
        }
        Err(err) => {
            if let Err(release_err) = uploads.release_upload(account_id, upload_id).await {
                tracing::warn!(
                    account_id = %account_id,
                    upload_id = %upload_id,
                    error = ?release_err,
                    "failed to release staged upload after {verb} failure"
                );
            }
            Err(err.into())
        }
    }
}
