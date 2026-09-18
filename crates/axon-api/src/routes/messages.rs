//! Mutation endpoints: send, edit, redact, and react.
//!
//! Each handler routes through the [`MessageSender`] port (injected via
//! `AppState`), turns its `event_id` result into a [`SendResultDto`], and maps a
//! [`SendError`](crate::sender::SendError) onto the shared error envelope. The
//! created event then round-trips back through sync and appears in the timeline
//! read and over `/v1/ws` — these handlers do not echo it.

use std::sync::Arc;

use axum::extract::State;
use uuid::Uuid;

use axon_core::{Formatted, MediaAttachment, Relation};

use crate::dto::{
    EditRequest, ReactRequest, RedactQuery, SendMediaRequest, SendMessageRequest, SendResultDto,
};
use crate::extract::{Json, Path, Query};
use crate::response::{ApiError, ApiResponse};
use crate::sender::MessageSender;
use crate::uploads::StagedUploadService;

/// The only `format` Matrix defines for `formatted_body`.
const HTML_FORMAT: &str = "org.matrix.custom.html";

/// Validate and pair a request's optional `format` / `formatted_body` into a
/// [`Formatted`]. Both must be supplied together (Matrix carries them as a pair),
/// `format` must be the one recognized markup, and the HTML body must be
/// non-empty — anything else is a `400` so a malformed request never reaches the
/// homeserver. Returns `Ok(None)` for a plain message (neither field set).
fn formatted<'a>(
    format: &'a Option<String>,
    formatted_body: &'a Option<String>,
) -> Result<Option<Formatted<'a>>, ApiError> {
    match (format.as_deref(), formatted_body.as_deref()) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => Err(ApiError::bad_request(
            "format and formatted_body must be provided together",
        )),
        (Some(format), Some(_)) if format != HTML_FORMAT => Err(ApiError::bad_request(format!(
            "unsupported format {format:?}; only {HTML_FORMAT:?} is supported"
        ))),
        (Some(_), Some("")) => Err(ApiError::bad_request("formatted_body must not be empty")),
        (Some(format), Some(body)) => Ok(Some(Formatted { format, body })),
    }
}

/// Send a plain-text message to a room.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/send",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    request_body = SendMessageRequest,
    responses(
        (status = 200, description = "Message sent", body = ApiResponse<SendResultDto>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
        (status = 504, description = "Timed out waiting on the SDK gateway (ADR 0030 decision #4, issue #241) — most likely the account's sync engine hasn't completed its first sync yet (see `AccountDto.sync_state`)", body = crate::response::ErrorResponse),
    ),
    tag = "messages",
)]
pub async fn send_message(
    State(sender): State<Arc<dyn MessageSender>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
    Json(req): Json<SendMessageRequest>,
) -> Result<ApiResponse<SendResultDto>, ApiError> {
    let fmt = formatted(&req.format, &req.formatted_body)?;
    let relation = Relation {
        reply_to: req.reply_to.as_deref(),
        thread_root: req.thread_root.as_deref(),
    };
    let event_id = sender
        .send_message(account_id, &room_id, &req.body, fmt, relation)
        .await?;
    Ok(ApiResponse::new(SendResultDto { event_id }))
}

/// Send a staged media upload to a room.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/send-media",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    request_body = SendMediaRequest,
    responses(
        (status = 200, description = "Media sent", body = ApiResponse<SendResultDto>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 404, description = "Account, room, or upload not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
    ),
    tag = "messages",
)]
pub async fn send_media(
    State(sender): State<Arc<dyn MessageSender>>,
    State(uploads): State<Arc<dyn StagedUploadService>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
    Json(req): Json<SendMediaRequest>,
) -> Result<ApiResponse<SendResultDto>, ApiError> {
    // Validate before claiming: a malformed request must not consume the upload.
    let fmt = formatted(&req.format, &req.formatted_body)?;
    // An empty caption is no caption, as on the edit path (`set_media_caption`
    // in `axon-sync`): the filename stays the event body. Formatting without a
    // caption has nothing to format, so it is rejected rather than sent as an
    // empty-but-formatted body.
    let caption = req.caption.as_deref().filter(|caption| !caption.is_empty());
    if fmt.is_some() && caption.is_none() {
        return Err(ApiError::bad_request(
            "format and formatted_body require a caption",
        ));
    }
    let relation = Relation {
        reply_to: req.reply_to.as_deref(),
        thread_root: req.thread_root.as_deref(),
    };
    let started_at = std::time::Instant::now();
    tracing::debug!(
        account_id = %account_id,
        room_id = %room_id,
        upload_id = %req.upload_id,
        "send_media: claiming staged upload"
    );
    let upload = uploads.claim_upload(account_id, req.upload_id).await?;
    tracing::debug!(
        account_id = %account_id,
        upload_id = %req.upload_id,
        size_bytes = upload.size_bytes,
        claim_elapsed_ms = started_at.elapsed().as_millis(),
        "send_media: upload claimed, handing off to the SDK attachment send"
    );
    let attachment: MediaAttachment = upload.into();

    let send_started_at = std::time::Instant::now();
    let send_result = sender
        .send_media(account_id, &room_id, attachment, caption, fmt, relation)
        .await;
    match send_result {
        Ok(event_id) => {
            tracing::info!(
                account_id = %account_id,
                room_id = %room_id,
                upload_id = %req.upload_id,
                event_id = %event_id,
                send_elapsed_ms = send_started_at.elapsed().as_millis(),
                total_elapsed_ms = started_at.elapsed().as_millis(),
                "send_media completed"
            );
            if let Err(err) = uploads.complete_upload(account_id, req.upload_id).await {
                tracing::warn!(
                    account_id = %account_id,
                    upload_id = %req.upload_id,
                    error = ?err,
                    "sent media but failed to consume staged upload"
                );
            }
            Ok(ApiResponse::new(SendResultDto { event_id }))
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                room_id = %room_id,
                upload_id = %req.upload_id,
                send_elapsed_ms = send_started_at.elapsed().as_millis(),
                error = ?err,
                "send_media: SDK attachment send failed"
            );
            if let Err(release_err) = uploads.release_upload(account_id, req.upload_id).await {
                tracing::warn!(
                    account_id = %account_id,
                    upload_id = %req.upload_id,
                    error = ?release_err,
                    "failed to release staged upload after media send failure"
                );
            }
            Err(err.into())
        }
    }
}

/// Edit an existing message (sends an `m.replace`).
#[utoipa::path(
    put,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
        ("event_id" = String, Path, description = "Event id being edited"),
    ),
    request_body = EditRequest,
    responses(
        (status = 200, description = "Edit sent", body = ApiResponse<SendResultDto>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
        (status = 504, description = "Timed out waiting on the SDK gateway (ADR 0030 decision #4, issue #241) — most likely the account's sync engine hasn't completed its first sync yet (see `AccountDto.sync_state`)", body = crate::response::ErrorResponse),
    ),
    tag = "messages",
)]
pub async fn edit_message(
    State(sender): State<Arc<dyn MessageSender>>,
    Path((account_id, room_id, event_id)): Path<(Uuid, String, String)>,
    Json(req): Json<EditRequest>,
) -> Result<ApiResponse<SendResultDto>, ApiError> {
    let fmt = formatted(&req.format, &req.formatted_body)?;
    let new_id = sender
        .edit(account_id, &room_id, &event_id, &req.body, fmt)
        .await?;
    Ok(ApiResponse::new(SendResultDto { event_id: new_id }))
}

/// Redact an event, optionally with a reason (`?reason=`).
#[utoipa::path(
    delete,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
        ("event_id" = String, Path, description = "Event id being redacted"),
        RedactQuery,
    ),
    responses(
        (status = 200, description = "Redaction sent", body = ApiResponse<SendResultDto>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
        (status = 504, description = "Timed out waiting on the SDK gateway (ADR 0030 decision #4, issue #241) — most likely the account's sync engine hasn't completed its first sync yet (see `AccountDto.sync_state`)", body = crate::response::ErrorResponse),
    ),
    tag = "messages",
)]
pub async fn redact_event(
    State(sender): State<Arc<dyn MessageSender>>,
    Path((account_id, room_id, event_id)): Path<(Uuid, String, String)>,
    Query(q): Query<RedactQuery>,
) -> Result<ApiResponse<SendResultDto>, ApiError> {
    let redaction_id = sender
        .redact(account_id, &room_id, &event_id, q.reason.as_deref())
        .await?;
    Ok(ApiResponse::new(SendResultDto {
        event_id: redaction_id,
    }))
}

/// React to an event with an emoji/short key.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}/reactions",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
        ("event_id" = String, Path, description = "Event id being reacted to"),
    ),
    request_body = ReactRequest,
    responses(
        (status = 200, description = "Reaction sent", body = ApiResponse<SendResultDto>),
        (status = 400, description = "Malformed request", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
        (status = 504, description = "Timed out waiting on the SDK gateway (ADR 0030 decision #4, issue #241) — most likely the account's sync engine hasn't completed its first sync yet (see `AccountDto.sync_state`)", body = crate::response::ErrorResponse),
    ),
    tag = "messages",
)]
pub async fn react(
    State(sender): State<Arc<dyn MessageSender>>,
    Path((account_id, room_id, event_id)): Path<(Uuid, String, String)>,
    Json(req): Json<ReactRequest>,
) -> Result<ApiResponse<SendResultDto>, ApiError> {
    let reaction_id = sender
        .react(account_id, &room_id, &event_id, &req.key)
        .await?;
    Ok(ApiResponse::new(SendResultDto {
        event_id: reaction_id,
    }))
}
