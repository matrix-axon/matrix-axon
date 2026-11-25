//! Preview-aware send orchestration. One owned task finishes bounded SDK/DB
//! work and claim bookkeeping even if the HTTP caller disconnects. A lost
//! response is still uncertain, never an invitation to replay the operation.

use std::{
    collections::HashSet,
    sync::{Arc, LazyLock},
};

use axon_core::{
    edit_preview::{EditPreview, PreviewEdit},
    link_preview::{self, LinkPreview, PreviewSend},
    Formatted, Relation,
};
use axon_store::Store;
use uuid::Uuid;

use crate::{
    dto::{
        EditPreviewRequest, EditRequest, LinkPreviewRequest, SendMediaRequest, SendMessageRequest,
    },
    response::{ApiError, ApiResponse},
    sender::MessageSender,
    uploads::StagedUploadService,
};

use super::media_send::{Claims, MediaFields, TxnGuard};
use super::messages::formatted;

static SEND_SLOTS: LazyLock<crate::AccountSlots> = LazyLock::new(crate::AccountSlots::default);

pub(super) struct Request {
    body: String,
    format: Option<String>,
    formatted_body: Option<String>,
    reply_to: Option<String>,
    thread_root: Option<String>,
    previews: Vec<LinkPreviewRequest>,
    media: Option<MediaFields>,
}

impl From<SendMessageRequest> for Request {
    fn from(r: SendMessageRequest) -> Self {
        Self {
            body: r.body,
            format: r.format,
            formatted_body: r.formatted_body,
            reply_to: r.reply_to,
            thread_root: r.thread_root,
            previews: r.url_previews.unwrap_or_default(),
            media: None,
        }
    }
}

impl From<SendMediaRequest> for Request {
    fn from(r: SendMediaRequest) -> Self {
        let media = MediaFields::from(&r);
        Self {
            body: r.caption.unwrap_or_default(),
            format: r.format,
            formatted_body: r.formatted_body,
            reply_to: r.reply_to,
            thread_root: r.thread_root,
            previews: r.url_previews.unwrap_or_default(),
            media: Some(media),
        }
    }
}

impl Request {
    fn relation(&self) -> Relation<'_> {
        Relation {
            reply_to: self.reply_to.as_deref(),
            thread_root: self.thread_root.as_deref(),
        }
    }
    fn formatted(&self) -> Option<Formatted<'_>> {
        self.formatted_body.as_deref().map(|body| Formatted {
            format: "org.matrix.custom.html",
            body,
        })
    }
    pub(super) fn validate(&self) -> Result<(), ApiError> {
        formatted(&self.format, &self.formatted_body)?;
        if self.media.is_some() && self.body.is_empty() && self.formatted_body.is_some() {
            return Err(ApiError::bad_request("formatting requires a caption"));
        }
        let mut ids = HashSet::new();
        if let Some(media) = &self.media {
            media.validate()?;
            ids.extend(media.upload_ids());
        }
        for p in &self.previews {
            if p.image_upload_id.is_some_and(|id| !ids.insert(id)) {
                return Err(ApiError::bad_request(
                    "upload IDs must be distinct within a send",
                ));
            }
        }
        link_preview::preflight(
            &self.body,
            self.formatted(),
            self.relation(),
            &self
                .previews
                .iter()
                .map(|p| (p.metadata(), p.image_upload_id.is_some()))
                .collect::<Vec<_>>(),
            self.media.is_some(),
        )
        .map_err(preview_error)
    }
}

fn preview_error(error: link_preview::PreviewError) -> ApiError {
    match error {
        link_preview::PreviewError::Invalid => {
            ApiError::bad_request("invalid link preview metadata or matched URL")
        }
        link_preview::PreviewError::PreviewsTooLarge => ApiError::preview_too_large(),
        link_preview::PreviewError::EventTooLarge => crate::sender::SendError::EventTooLarge.into(),
    }
}

/// `txn` is the `send-media` request's `txn_id` guard, if it has one, which
/// the task settles with the send (ADR 0142 section 4).
pub(super) async fn send(
    sender: Arc<dyn MessageSender>,
    uploads: Arc<dyn StagedUploadService>,
    account_id: Uuid,
    room_id: String,
    request: Request,
    txn: Option<TxnGuard>,
    thumbnailer: Option<Arc<dyn crate::SendThumbnailer>>,
) -> Result<ApiResponse<crate::dto::SendResultDto>, ApiError> {
    // No upload claims or SDK work on invalid input.
    request.validate()?;
    // Metadata-only text sends (including []) claim no bytes and bypass the
    // upload gate. They still emit the explicit preview field and share preflight.
    let has_uploads =
        request.media.is_some() || request.previews.iter().any(|p| p.image_upload_id.is_some());
    let permit = if has_uploads {
        Some(acquire_slot(account_id).await?)
    } else {
        None
    };
    tokio::spawn(async move {
        let _permit = permit;
        let started = std::time::Instant::now();
        tracing::debug!(%account_id, %room_id, preview_count = request.previews.len(), "preview-aware send started");
        let mut claims = Claims::new(uploads.clone(), account_id, &room_id);
        let result = async {
            let previews = claim_previews(&*uploads, account_id, &request.previews, &mut claims.ids).await?;
            let attachment = match &request.media {
                Some(media) => {
                    let attachment = super::media_send::claim_attachment(
                        &*uploads, account_id, media, &mut claims.ids,
                    ).await?;
                    Some(super::media_send::inspect_image(attachment, thumbnailer, account_id, &room_id, media))
                }
                None => None,
            };
            let message = PreviewSend {
                body: &request.body,
                formatted: request.formatted(),
                relation: request.relation(),
                previews,
            };
            let txn_id = request.media.as_ref().and_then(|m| m.txn_id.as_deref());
            sender
                .send_with_previews(account_id, &room_id, message, attachment, txn_id)
                .await
                .map_err(ApiError::from)
        }
        .await;
        claims.settle(result.is_ok()).await;
        if let Some(txn) = txn {
            txn.settle(&result);
        }
        match result {
            Ok(event_id) => {
                tracing::info!(%account_id, %room_id, %event_id, preview_count = request.previews.len(), elapsed_ms = started.elapsed().as_millis(), "sent message with bundled previews");
                Ok(ApiResponse::new(crate::dto::SendResultDto { event_id }))
            }
            Err(error) => {
                let (status, code) = error.diagnostics();
                tracing::warn!(%account_id, %room_id, elapsed_ms = started.elapsed().as_millis(), status, code, "preview-aware send failed; upload cleanup attempted");
                Err(error)
            }
        }
    })
    .await
    .map_err(|_| ApiError::internal())?
}

/// Claim each preview's image, in order, recording every claim in `claimed`
/// so the caller settles it whatever happens next.
async fn claim_previews(
    uploads: &dyn StagedUploadService,
    account_id: Uuid,
    requests: &[LinkPreviewRequest],
    claimed: &mut Vec<Uuid>,
) -> Result<Vec<LinkPreview>, ApiError> {
    let mut previews = Vec::with_capacity(requests.len());
    for p in requests {
        let image = if let Some(id) = p.image_upload_id {
            let image = uploads.claim_preview_upload(account_id, id).await?;
            claimed.push(id);
            Some(image)
        } else {
            None
        };
        previews.push(LinkPreview {
            metadata: p.metadata(),
            image,
        });
    }
    Ok(previews)
}

/// An edit (ADR 0132 section 8), validated before any claim or store read.
pub(super) async fn edit(
    sender: Arc<dyn MessageSender>,
    uploads: Arc<dyn StagedUploadService>,
    store: Store,
    account_id: Uuid,
    room_id: String,
    event_id: String,
    request: EditRequest,
) -> Result<ApiResponse<crate::dto::SendResultDto>, ApiError> {
    formatted(&request.format, &request.formatted_body)?;
    let formatted_body = request.formatted_body.as_deref().map(|body| Formatted {
        format: "org.matrix.custom.html",
        body,
    });
    if let Some(previews) = &request.url_previews {
        validate_edit_previews(&request.body, formatted_body, previews)?;
    }
    let new: Vec<LinkPreviewRequest> = request
        .url_previews
        .iter()
        .flatten()
        .filter_map(|p| match p {
            EditPreviewRequest::New(p) => Some(p.clone()),
            EditPreviewRequest::Keep(_) => None,
        })
        .collect();
    let permit = if new.iter().any(|p| p.image_upload_id.is_some()) {
        Some(acquire_slot(account_id).await?)
    } else {
        None
    };
    tokio::spawn(async move {
        let _permit = permit;
        let mut claims = Claims::new(uploads.clone(), account_id, &room_id);
        let result = async {
            let mut claimed_new =
                claim_previews(&*uploads, account_id, &new, &mut claims.ids).await?.into_iter();
            let previews = request.url_previews.as_ref().map(|list| {
                list.iter()
                    .filter_map(|p| match p {
                        EditPreviewRequest::Keep(k) => Some(EditPreview::Keep {
                            matched_url: k.matched_url.clone(),
                        }),
                        EditPreviewRequest::New(_) => claimed_new.next().map(EditPreview::New),
                    })
                    .collect()
            });
            // The message as it stands, with its latest edit applied, read as
            // late as possible: after validation, any wait for an upload slot,
            // and the claims. Absent from the store, the sender uses the
            // original event instead.
            let current = store
                .get_event(account_id, &event_id)
                .await?
                .filter(|row| row.room_id == room_id)
                .and_then(|row| row.content);
            let edit = PreviewEdit {
                body: &request.body,
                formatted: request.formatted_body.as_deref().map(|body| Formatted {
                    format: "org.matrix.custom.html",
                    body,
                }),
                previews,
                current,
            };
            sender
                .edit(account_id, &room_id, &event_id, edit)
                .await
                .map_err(ApiError::from)
        }
        .await;
        claims.settle(result.is_ok()).await;
        if let Err(error) = &result {
            let (status, code) = error.diagnostics();
            tracing::debug!(%account_id, %room_id, status, code, "edit failed; upload cleanup attempted");
        }
        result.map(|event_id| ApiResponse::new(crate::dto::SendResultDto { event_id }))
    })
    .await
    .map_err(|_| ApiError::internal())?
}

/// An edit's explicit list, checked before any claim: `keep` is `true`, upload
/// ids are distinct, every link is a complete bare link in the new body and a
/// distinct destination, and new entries are within the send limits. The
/// size of the whole replacement, with kept entries, is checked by the sender.
fn validate_edit_previews(
    body: &str,
    formatted_body: Option<Formatted<'_>>,
    previews: &[EditPreviewRequest],
) -> Result<(), ApiError> {
    let mut ids = HashSet::new();
    let mut all = Vec::with_capacity(previews.len());
    let mut new = Vec::new();
    for p in previews {
        match p {
            EditPreviewRequest::Keep(k) => {
                if !k.keep {
                    return Err(ApiError::bad_request(
                        "a preview entry with `keep` must set it to true",
                    ));
                }
                all.push(link_preview::PreviewMetadata {
                    matched_url: k.matched_url.clone(),
                    ..Default::default()
                });
            }
            EditPreviewRequest::New(p) => {
                if p.image_upload_id.is_some_and(|id| !ids.insert(id)) {
                    return Err(ApiError::bad_request(
                        "upload IDs must be distinct within an edit",
                    ));
                }
                all.push(p.metadata());
                new.push((p.metadata(), p.image_upload_id.is_some()));
            }
        }
    }
    link_preview::validate_metadata(body, &all).map_err(preview_error)?;
    // A lower bound: the replacement carries the body twice and kept entries
    // too, which the sender checks once the message's previews are known.
    link_preview::preflight(body, formatted_body, Relation::default(), &new, false)
        .map_err(preview_error)
}

async fn acquire_slot(account_id: Uuid) -> Result<tokio::sync::OwnedSemaphorePermit, ApiError> {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        SEND_SLOTS.for_account(account_id).acquire_owned(),
    )
    .await
    .map_err(|_| ApiError::service_unavailable("preview send capacity timed out"))?
    .map_err(|_| ApiError::service_unavailable("preview send capacity unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    #[tokio::test]
    async fn portable_contract_fixtures_match_the_production_dto_and_preflight() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/link-preview-contract.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let decoded = serde_json::from_value::<SendMessageRequest>(case["request"].clone());
            let result = decoded
                .map_err(|_| ApiError::bad_request("invalid request"))
                .and_then(|r| {
                    if r.url_previews.is_none() {
                        Ok(())
                    } else {
                        Request::from(r).validate()
                    }
                });
            let status = match result {
                Ok(()) => 200,
                Err(error) => {
                    let response = error.into_response();
                    let status = response.status().as_u16();
                    if let Some(code) = case.get("code") {
                        let bytes = axum::body::to_bytes(response.into_body(), 4096)
                            .await
                            .unwrap();
                        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                        assert_eq!(&body["error"]["code"], code, "{}", case["name"]);
                    }
                    status
                }
            };
            assert_eq!(
                status,
                case["status"].as_u64().unwrap() as u16,
                "{}",
                case["name"]
            );
        }
    }
}
