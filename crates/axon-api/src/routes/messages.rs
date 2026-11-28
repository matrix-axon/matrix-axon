//! Mutation endpoints: send, edit, redact, and react.
//!
//! Each handler routes through the [`MessageSender`] port (injected via
//! `AppState`), turns its `event_id` result into a [`SendResultDto`], and maps a
//! [`SendError`](crate::sender::SendError) onto the shared error envelope. The
//! created event then round-trips back through sync and appears in the timeline
//! read and over `/v1/ws` — these handlers do not echo it.

use std::sync::Arc;

use axon_store::Store;
use axum::extract::State;
use uuid::Uuid;

use axon_core::{Formatted, Relation};

use crate::dto::{
    EditRequest, ReactRequest, RedactQuery, SendMediaRequest, SendMessageRequest, SendResultDto,
};
use crate::extract::{Json, Path, Query};
use crate::response::{ApiError, ApiResponse};
use crate::sender::MessageSender;
use crate::uploads::StagedUploadService;

use super::media_send::{self, MediaFields};

/// The only `format` Matrix defines for `formatted_body`.
const HTML_FORMAT: &str = "org.matrix.custom.html";

/// Send a sticker
///
/// Send an existing MXC image as a standalone Matrix `m.sticker` event.
/// Info accepts raster MIME types, dimensions 1–16,384, and sizes at most
/// 100 MiB. Empty info is permitted; bytes are never fetched during a send.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/send-sticker",
    params(("account_id" = Uuid, Path), ("room_id" = String, Path)),
    request_body = crate::dto::SendStickerRequest,
    responses(
        (status = 200, description = "Sticker sent", body = ApiResponse<SendResultDto>),
        (status = 400, description = "Invalid sticker", body = crate::response::ErrorResponse),
        (status = 401, description = "Unauthorized", body = crate::response::ErrorResponse),
        (status = 403, description = "Forbidden", body = crate::response::ErrorResponse),
        (status = 404, description = "Account or room not found", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream failure", body = crate::response::ErrorResponse),
        (status = 503, description = "Account unavailable", body = crate::response::ErrorResponse),
        (status = 504, description = "Send deadline exceeded", body = crate::response::ErrorResponse),
    ),
    tag = "messages"
)]
pub async fn send_sticker(
    State(sender): State<Arc<dyn MessageSender>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
    Json(req): Json<crate::dto::SendStickerRequest>,
) -> Result<ApiResponse<SendResultDto>, ApiError> {
    let sticker = axon_core::image_pack::Sticker {
        body: req.body,
        url: req.url,
        info: serde_json::to_value(req.info.unwrap_or_default())
            .map_err(|_| ApiError::bad_request("invalid sticker info"))?,
    };
    if !sticker.validate() {
        return Err(ApiError::bad_request(
            "invalid sticker URL, body, or image metadata",
        ));
    }
    let relation = Relation {
        reply_to: req.reply_to.as_deref(),
        thread_root: req.thread_root.as_deref(),
    };
    let event_id = sender
        .send_sticker(account_id, &room_id, &sticker, relation)
        .await?;
    tracing::info!(%account_id, %room_id, %event_id, "sticker sent");
    Ok(ApiResponse::new(SendResultDto { event_id }))
}

/// Validate and pair a request's optional `format` / `formatted_body` into a
/// [`Formatted`]. Both must be supplied together (Matrix carries them as a pair),
/// `format` must be the one recognized markup, and the HTML body must be
/// non-empty — anything else is a `400` so a malformed request never reaches the
/// homeserver. Returns `Ok(None)` for a plain message (neither field set).
pub(super) fn formatted<'a>(
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

/// Send a message
///
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
        (status = 404, description = "Account, room or preview upload not found", body = crate::response::ErrorResponse),
        (status = 413, description = "Definite validation rejection: event_too_large (40960-byte content budget), preview_too_large (32768-byte preview budget), or payload_too_large (thumbnail bytes). No event dispatched by preflight; preserve the draft. An explicit homeserver M_TOO_LARGE is also event_too_large.", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
        (status = 504, description = "Timed out waiting on the SDK gateway (ADR 0030 decision #4, issue #241) — most likely the account's sync engine hasn't completed its first sync yet (see `AccountDto.sync_state`)", body = crate::response::ErrorResponse),
    ),
    tag = "messages",
)]
pub async fn send_message(
    State(sender): State<Arc<dyn MessageSender>>,
    State(uploads): State<Arc<dyn StagedUploadService>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
    Json(req): Json<SendMessageRequest>,
) -> Result<ApiResponse<SendResultDto>, ApiError> {
    if req.url_previews.is_some() {
        return super::bundled_previews::send(
            sender,
            uploads,
            account_id,
            room_id,
            req.into(),
            None,
            None,
        )
        .await;
    }
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

/// Send media
///
/// Send a staged media upload to a room, as the event its staged kind names,
/// with optional `info` and a thumbnail (`thumbnail_upload_id`). A send with a
/// `txn_id` already in flight or sent within 10 minutes in this room returns
/// that send's event id and sends nothing (ADR 0142 section 4).
///
/// The send runs in a task of its own, which settles every upload it
/// claimed even if the client disconnects: consumed when the event is sent,
/// released when it fails. Each step has its own budget (ADR 0142 section
/// 8): preparing a supplied thumbnail 5 s; resolving the room and uploading
/// the original `media.upstream_upload_timeout_secs` (60 s by default); the
/// thumbnail's upload, which is best-effort, 10 s; and building and sending
/// the event, including any reply lookup, `sync.send_mutation_timeout_secs`
/// (30 s by default). A request with `url_previews` first waits up to 5 s
/// for a preview slot. So Axon answers within 105 s with the defaults, or
/// 110 s with `url_previews`, plus the time to read the staged files from
/// disk. A client should wait longer than that, allowing for the network,
/// and send a `txn_id`, so that a retry after it gives up joins the send
/// instead of posting it again. For images, Axon probes dimensions and can
/// generate a thumbnail while the original uploads, within a 5 s budget of
/// its own and the original's step: inspection still running when that
/// step's budget ends is given up, and the image goes without what it would
/// have added.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/send-media",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    request_body = SendMediaRequest,
    responses(
        (status = 200, description = "Media sent, or the event id of the earlier send this txn_id names", body = ApiResponse<SendResultDto>),
        (status = 400, description = "Malformed request: invalid info or txn_id, a field or thumbnail the upload's kind cannot carry, or a thumbnail that is not a valid image. Every claimed upload is released.", body = crate::response::ErrorResponse),
        (status = 413, description = "Definite event_too_large, preview_too_large or payload_too_large validation rejection; preview-aware content uses a 40960-byte budget including 12288 bytes reserved for media fields. payload_too_large also covers a staged upload that is now over the account's `media_upload_limit.max_bytes`, which can drop after staging: its claim is released, the upload stays staged and retryable, and nothing is sent. Otherwise no preflight upload claims or event dispatch.", body = crate::response::ErrorResponse),
        (status = 409, description = "txn_id was already used for a different send in this room. Nothing was sent and the request's uploads are kept.", body = crate::response::ErrorResponse),
        (status = 504, description = "Send deadline exceeded: outcome unknown, or a retry waited too long for the earlier send with its txn_id. Resend only with the same txn_id: Axon does not remember a timed-out send, so the retry uploads again, but it reuses the Matrix transaction id, so a homeserver that completed the first send returns that event instead of posting another.", body = crate::response::ErrorResponse),
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
    State(thumbnailer): State<Option<Arc<dyn crate::SendThumbnailer>>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
    Json(req): Json<SendMediaRequest>,
) -> Result<ApiResponse<SendResultDto>, ApiError> {
    // Validate before claiming: a malformed request must not consume an upload.
    let fields = MediaFields::from(&req);
    fields.validate()?;
    if req.url_previews.is_some() {
        let request = super::bundled_previews::Request::from(req);
        request.validate()?;
        let task_uploads = uploads.clone();
        return media_send::deduplicated(&*uploads, account_id, &room_id, &fields, |txn| {
            super::bundled_previews::send(
                sender,
                task_uploads,
                account_id,
                room_id.clone(),
                request,
                txn,
                thumbnailer,
            )
        })
        .await;
    }
    formatted(&req.format, &req.formatted_body)?;
    // An empty caption is no caption, as on the edit path (`set_media_caption`
    // in `axon-sync`): the filename stays the event body. Formatting without a
    // caption has nothing to format, so it is rejected rather than sent as an
    // empty-but-formatted body.
    let caption = req.caption.filter(|caption| !caption.is_empty());
    if req.formatted_body.is_some() && caption.is_none() {
        return Err(ApiError::bad_request(
            "format and formatted_body require a caption",
        ));
    }
    let formatted_body = req.formatted_body;
    let (reply_to, thread_root) = (req.reply_to, req.thread_root);
    let task_uploads = uploads.clone();
    let task_room = room_id.clone();
    let task_fields = fields.clone();
    media_send::deduplicated(&*uploads, account_id, &room_id, &fields, |txn| async move {
        let (uploads, room_id, fields) = (task_uploads, task_room, task_fields);
        // One task owns every claim, the send and the settlement, so a client
        // that disconnects cannot strand an upload as `sending` (ADR 0142
        // section 4). The request only waits for it.
        tokio::spawn(async move {
            let started_at = std::time::Instant::now();
            let mut claims = media_send::Claims::new(uploads.clone(), account_id, &room_id);
            let result = async {
                let attachment =
                    media_send::claim_attachment(&*uploads, account_id, &fields, &mut claims.ids)
                        .await?;
                tracing::debug!(
                    %account_id,
                    upload_id = %fields.upload_id,
                    size_bytes = attachment.size_bytes,
                    thumbnail = attachment.thumbnail.is_some(),
                    claim_elapsed_ms = started_at.elapsed().as_millis(),
                    "send_media: claimed, handing off to the media send"
                );
                let relation = Relation {
                    reply_to: reply_to.as_deref(),
                    thread_root: thread_root.as_deref(),
                };
                let fmt = formatted_body.as_deref().map(|body| Formatted {
                    format: HTML_FORMAT,
                    body,
                });
                let send = media_send::inspect_image(
                    attachment,
                    thumbnailer,
                    account_id,
                    &room_id,
                    &fields,
                );
                sender
                    .send_media(
                        account_id,
                        &room_id,
                        send,
                        caption.as_deref(),
                        fmt,
                        relation,
                        fields.txn_id.as_deref(),
                    )
                    .await
                    .map_err(ApiError::from)
            }
            .await;
            let sent = result.is_ok();
            claims.settle(sent).await;
            if let Some(txn) = txn {
                txn.settle(&result);
            }
            match result {
                Ok(event_id) => {
                    tracing::info!(
                        %account_id,
                        %room_id,
                        upload_id = %fields.upload_id,
                        %event_id,
                        total_elapsed_ms = started_at.elapsed().as_millis(),
                        "send_media completed"
                    );
                    Ok(ApiResponse::new(SendResultDto { event_id }))
                }
                Err(error) => {
                    let (status, code) = error.diagnostics();
                    tracing::warn!(
                        %account_id,
                        %room_id,
                        upload_id = %fields.upload_id,
                        elapsed_ms = started_at.elapsed().as_millis(),
                        status,
                        code,
                        "send_media failed; uploads released"
                    );
                    Err(error)
                }
            }
        })
        .await
        .map_err(|_| ApiError::internal())?
    })
    .await
}

/// Edit a message
///
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
        (status = 400, description = "Malformed request, or invalid preview metadata or image", body = crate::response::ErrorResponse),
        (status = 403, description = "Operation not permitted", body = crate::response::ErrorResponse),
        (status = 404, description = "Account, room or preview upload not found", body = crate::response::ErrorResponse),
        (status = 409, description = "previews_changed: a `keep` entry names a link the message has no preview for, because another device's edit removed it. Nothing was sent; reload the message and edit again.", body = crate::response::ErrorResponse),
        (status = 413, description = "An edit that sets url_previews: event_too_large (40960-byte budget for the whole replacement), preview_too_large (32768-byte preview budget), or payload_too_large (thumbnail bytes). Nothing was sent.", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
        (status = 503, description = "Account not reachable", body = crate::response::ErrorResponse),
        (status = 504, description = "Timed out waiting on the SDK gateway (ADR 0030 decision #4, issue #241) — most likely the account's sync engine hasn't completed its first sync yet (see `AccountDto.sync_state`)", body = crate::response::ErrorResponse),
    ),
    tag = "messages",
)]
pub async fn edit_message(
    State(sender): State<Arc<dyn MessageSender>>,
    State(uploads): State<Arc<dyn StagedUploadService>>,
    State(store): State<Store>,
    Path((account_id, room_id, event_id)): Path<(Uuid, String, String)>,
    Json(req): Json<EditRequest>,
) -> Result<ApiResponse<SendResultDto>, ApiError> {
    super::bundled_previews::edit(sender, uploads, store, account_id, room_id, event_id, req).await
}

/// Redact an event
///
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

/// React to an event
///
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
    let shortcode = req.shortcode.as_deref().map(|name| {
        if axon_core::media::parse_mxc(&req.key).is_none() {
            return Err(ApiError::bad_request("shortcode requires a valid MXC reaction key"));
        }
        axon_core::image_pack::reaction_shortcode(name)
            .ok_or_else(|| ApiError::bad_request("shortcode must be nonblank, at most 100 UTF-8 bytes, without control characters"))
    }).transpose()?;
    let reaction_id = sender
        .react_with_shortcode(account_id, &room_id, &event_id, &req.key, shortcode)
        .await?;
    Ok(ApiResponse::new(SendResultDto {
        event_id: reaction_id,
    }))
}
