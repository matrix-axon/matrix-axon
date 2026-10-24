//! Preview-aware send orchestration. One owned task finishes bounded SDK/DB
//! work and claim bookkeeping even if the HTTP caller disconnects. A lost
//! response is still uncertain, never an invitation to replay the operation.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock, Mutex, Weak},
};

use axon_core::{
    link_preview::{self, LinkPreview, PreviewSend},
    Formatted, Relation,
};
use uuid::Uuid;

use crate::{
    dto::{LinkPreviewRequest, SendMediaRequest, SendMessageRequest},
    response::{ApiError, ApiResponse},
    sender::MessageSender,
    uploads::StagedUploadService,
};

use super::messages::formatted;

// Weak entries are pruned under the registry lock; only active sends/waiters
// keep capacity alive. A slow homeserver cannot consume another account's slots.
#[derive(Default)]
struct SendSlots(Mutex<HashMap<Uuid, Weak<tokio::sync::Semaphore>>>);

impl SendSlots {
    fn for_account(&self, account: Uuid) -> Arc<tokio::sync::Semaphore> {
        let mut slots = self.0.lock().expect("preview capacity registry poisoned");
        slots.retain(|_, value| value.strong_count() > 0);
        slots
            .entry(account)
            .or_default()
            .upgrade()
            .unwrap_or_else(|| {
                let semaphore = Arc::new(tokio::sync::Semaphore::new(4));
                slots.insert(account, Arc::downgrade(&semaphore));
                semaphore
            })
    }
}

static SEND_SLOTS: LazyLock<SendSlots> = LazyLock::new(SendSlots::default);

pub(super) struct Request {
    body: String,
    format: Option<String>,
    formatted_body: Option<String>,
    reply_to: Option<String>,
    thread_root: Option<String>,
    previews: Vec<LinkPreviewRequest>,
    attachment_id: Option<Uuid>,
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
            attachment_id: None,
        }
    }
}

impl From<SendMediaRequest> for Request {
    fn from(r: SendMediaRequest) -> Self {
        Self {
            body: r.caption.unwrap_or_default(),
            format: r.format,
            formatted_body: r.formatted_body,
            reply_to: r.reply_to,
            thread_root: r.thread_root,
            previews: r.url_previews.unwrap_or_default(),
            attachment_id: Some(r.upload_id),
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
    fn validate(&self) -> Result<(), ApiError> {
        formatted(&self.format, &self.formatted_body)?;
        if self.attachment_id.is_some() && self.body.is_empty() && self.formatted_body.is_some() {
            return Err(ApiError::bad_request("formatting requires a caption"));
        }
        let mut ids = HashSet::new();
        if let Some(id) = self.attachment_id {
            ids.insert(id);
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
            self.attachment_id.is_some(),
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

pub(super) async fn send(
    sender: Arc<dyn MessageSender>,
    uploads: Arc<dyn StagedUploadService>,
    account_id: Uuid,
    room_id: String,
    request: Request,
) -> Result<ApiResponse<crate::dto::SendResultDto>, ApiError> {
    request.validate()?; // No upload claims or SDK work on invalid input.
                         // Metadata-only text sends (including []) claim no bytes and bypass the
                         // upload gate. They still emit the explicit preview field and share preflight.
    let has_uploads = request.attachment_id.is_some()
        || request.previews.iter().any(|p| p.image_upload_id.is_some());
    let permit = if has_uploads {
        Some(
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                SEND_SLOTS.for_account(account_id).acquire_owned(),
            )
            .await
            .map_err(|_| ApiError::service_unavailable("preview send capacity timed out"))?
            .map_err(|_| ApiError::service_unavailable("preview send capacity unavailable"))?,
        )
    } else {
        None
    };
    tokio::spawn(async move {
        let _permit = permit;
        let started = std::time::Instant::now();
        tracing::debug!(%account_id, %room_id, preview_count = request.previews.len(), "preview-aware send started");
        let mut claimed = Vec::new();
        let result = async {
            let mut previews = Vec::with_capacity(request.previews.len());
            for p in &request.previews {
                let image = if let Some(id) = p.image_upload_id {
                    let image = uploads.claim_preview_upload(account_id, id).await?;
                    claimed.push(id);
                    Some(image)
                } else { None };
                previews.push(LinkPreview { metadata: p.metadata(), image });
            }
            let attachment = if let Some(id) = request.attachment_id {
                let upload = uploads.claim_upload(account_id, id).await?;
                claimed.push(id);
                Some(upload.into())
            } else { None };
            let message = PreviewSend { body: &request.body, formatted: request.formatted(), relation: request.relation(), previews };
            sender.send_with_previews(account_id, &room_id, message, attachment).await.map_err(ApiError::from)
        }.await;
        for id in claimed {
            let cleanup = if result.is_ok() { uploads.complete_upload(account_id, id).await } else { uploads.release_upload(account_id, id).await };
            if let Err(error) = cleanup {
                // Never format error detail: it can contain private input.
                let kind = match error {
                    crate::uploads::StageUploadError::Invalid(_) => "invalid",
                    crate::uploads::StageUploadError::NotFound(_) => "not_found",
                    crate::uploads::StageUploadError::Forbidden(_) => "forbidden",
                    crate::uploads::StageUploadError::TooLarge { .. } => "too_large",
                    crate::uploads::StageUploadError::Timeout(_) => "timeout",
                    crate::uploads::StageUploadError::Internal(_) => "internal",
                };
                tracing::warn!(%account_id, %room_id, upload_id = %id, sent = result.is_ok(), error_kind = kind, "preview send upload bookkeeping failed; boot reconciliation/expiry will recover");
            }
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
            },
        }
    }).await.map_err(|_| ApiError::internal())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    #[test]
    fn upload_capacity_is_isolated_by_account_and_idle_entries_expire() {
        let slots = SendSlots::default();
        let account = Uuid::new_v4();
        let first = slots.for_account(account);
        let held = (0..4)
            .map(|_| first.clone().try_acquire_owned().unwrap())
            .collect::<Vec<_>>();
        assert!(slots.for_account(account).try_acquire_owned().is_err());
        let other = slots
            .for_account(Uuid::new_v4())
            .try_acquire_owned()
            .unwrap();
        assert_eq!(slots.0.lock().unwrap().len(), 2);
        drop(held);
        drop(first);
        drop(other);
        let current = slots.for_account(Uuid::new_v4());
        assert_eq!(slots.0.lock().unwrap().len(), 1);
        assert_eq!(current.available_permits(), 4);
    }

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
