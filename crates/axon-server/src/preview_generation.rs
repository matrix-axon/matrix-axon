//! Compose homeserver acquisition, shared raster validation and durable staging.
use crate::uploads::{validate_preview_image, FilesystemStagedUploads};
use async_trait::async_trait;
use axon_api::{
    ApiError, GeneratedPreviewDto, LinkPreviewRequest, MediaUploadKindDto,
    PreviewGenerationStatus as Status, PreviewGenerator, PreviewThumbnailStatus as Thumbnail,
    StagedUploadService,
};
use axon_sync::{HomeserverPreviewGenerator, PreviewFailure};
use std::sync::Arc;
use tokio::sync::Semaphore;
use uuid::Uuid;

#[async_trait]
trait PreviewSource: Send + Sync {
    async fn acquire(
        &self,
        account_id: Uuid,
        url: &str,
    ) -> Result<axon_sync::AcquiredPreview, PreviewFailure>;
}
#[async_trait]
impl PreviewSource for HomeserverPreviewGenerator {
    async fn acquire(
        &self,
        account_id: Uuid,
        url: &str,
    ) -> Result<axon_sync::AcquiredPreview, PreviewFailure> {
        HomeserverPreviewGenerator::acquire(self, account_id, url).await
    }
}

pub struct PreviewGenerationService {
    source: Arc<dyn PreviewSource>,
    uploads: Arc<FilesystemStagedUploads>,
    // No unbounded work queue. The owned decode task holds its permit until
    // completion, including when its HTTP caller disappears.
    capacity: Arc<Semaphore>,
}
impl PreviewGenerationService {
    pub fn new(source: HomeserverPreviewGenerator, uploads: Arc<FilesystemStagedUploads>) -> Self {
        Self {
            source: Arc::new(source),
            uploads,
            capacity: Arc::new(Semaphore::new(4)),
        }
    }
}
#[async_trait]
impl PreviewGenerator for PreviewGenerationService {
    async fn generate(
        &self,
        account_id: Uuid,
        url: String,
    ) -> Result<GeneratedPreviewDto, ApiError> {
        let Ok(permit) = self.capacity.clone().try_acquire_owned() else {
            return Ok(GeneratedPreviewDto::fallback(Status::Busy));
        };
        let acquired = match self.source.acquire(account_id, &url).await {
            Ok(value) => value,
            Err(failure) => {
                let status = match failure {
                    PreviewFailure::Unsupported => Status::Unsupported,
                    PreviewFailure::Disabled => Status::Disabled,
                    PreviewFailure::Timeout => Status::Timeout,
                    PreviewFailure::Unavailable => Status::Unavailable,
                };
                tracing::debug!(%account_id, ?status, "homeserver preview generation fallback");
                return Ok(GeneratedPreviewDto::fallback(status));
            }
        };
        let mut thumbnail_status = if acquired.image_unavailable {
            Thumbnail::Unavailable
        } else {
            Thumbnail::Absent
        };
        let mut image_upload_id = None;
        let mut image_expires_at = None;
        if let Some(bytes) = acquired.image {
            let (_permit, image) = tokio::task::spawn_blocking(move || {
                let image = validate_preview_image(axon_api::ClaimedUpload {
                    upload_id: Uuid::nil(),
                    kind: MediaUploadKindDto::Image,
                    filename: "preview".into(),
                    content_type: None,
                    size_bytes: bytes.len() as u64,
                    bytes,
                });
                (permit, image)
            })
            .await
            .map_err(|_| ApiError::internal())?;
            thumbnail_status = Thumbnail::Unavailable;
            if let Ok(image) = image {
                let mime = image.mimetype;
                let request = axon_api::StageUploadRequest {
                    account_id,
                    kind: MediaUploadKindDto::Image,
                    filename: "preview".into(),
                    content_type: Some(mime),
                };
                let body = Box::pin(futures_util::stream::once(async { Ok(image.bytes.into()) }));
                // Also bound the existing staging semaphore's queue wait.
                match tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    self.uploads.stage_upload(request, body),
                )
                .await
                {
                    Ok(Ok(staged)) => {
                        image_upload_id = Some(staged.upload_id);
                        image_expires_at = Some(staged.expires_at);
                        thumbnail_status = Thumbnail::Ready;
                    }
                    _ => {
                        tracing::warn!(%account_id, "generated preview thumbnail staging unavailable")
                    }
                }
            }
        }
        let p = acquired.metadata;
        if p.title.is_none()
            && p.description.is_none()
            && p.site_name.is_none()
            && image_upload_id.is_none()
        {
            let mut result = GeneratedPreviewDto::fallback(Status::NoMetadata);
            result.thumbnail_status = thumbnail_status;
            return Ok(result);
        }
        tracing::debug!(%account_id, ?thumbnail_status, "homeserver preview generated");
        Ok(GeneratedPreviewDto {
            status: Status::Ready,
            preview: Some(LinkPreviewRequest {
                matched_url: p.matched_url,
                title: p.title,
                description: p.description,
                site_name: p.site_name,
                image_upload_id,
            }),
            thumbnail_status,
            image_expires_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct BlockedSource {
        entered: tokio::sync::mpsc::Sender<()>,
        release: Semaphore,
    }
    #[async_trait]
    impl PreviewSource for BlockedSource {
        async fn acquire(
            &self,
            _: Uuid,
            url: &str,
        ) -> Result<axon_sync::AcquiredPreview, PreviewFailure> {
            self.entered.send(()).await.unwrap();
            let _permit = self.release.acquire().await.unwrap();
            Ok(axon_sync::AcquiredPreview {
                metadata: axon_core::link_preview::PreviewMetadata {
                    matched_url: url.into(),
                    title: Some("Generated title".into()),
                    description: None,
                    site_name: None,
                },
                image: None,
                image_unavailable: true,
            })
        }
    }
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn preview_generation_capacity_is_bounded_without_queueing_and_recovers() {
        let store = axon_store::Store::connect(&std::env::var("DATABASE_URL").unwrap(), 5)
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let uploads = Arc::new(
            FilesystemStagedUploads::new(
                store,
                &axon_core::MediaConfig {
                    uploads_dir: dir.path().into(),
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        let (entered, mut rx) = tokio::sync::mpsc::channel(4);
        let source = Arc::new(BlockedSource {
            entered,
            release: Semaphore::new(0),
        });
        let service = Arc::new(PreviewGenerationService {
            source: source.clone(),
            uploads,
            capacity: Arc::new(Semaphore::new(4)),
        });
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let service = service.clone();
            tasks.push(tokio::spawn(async move {
                service
                    .generate(Uuid::nil(), "https://example.org".into())
                    .await
                    .unwrap()
            }));
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
        }
        let busy = service
            .generate(Uuid::nil(), "https://example.org".into())
            .await
            .unwrap();
        assert_eq!(busy.status, Status::Busy);
        assert!(busy.preview.is_none());
        source.release.add_permits(4);
        for task in tasks {
            let result = task.await.unwrap();
            assert_eq!(result.status, Status::Ready);
            assert_eq!(result.thumbnail_status, Thumbnail::Unavailable);
            assert_eq!(
                result.preview.unwrap().title.as_deref(),
                Some("Generated title")
            );
        }
        assert_eq!(service.capacity.available_permits(), 4);
        assert_eq!(
            service
                .generate(Uuid::nil(), "https://example.org".into())
                .await
                .unwrap()
                .status,
            Status::Ready
        );
    }
}
