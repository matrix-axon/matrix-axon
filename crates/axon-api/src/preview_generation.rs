//! Explicit preview-generation port. No receipt/render/send path calls it.
use crate::{dto::GeneratedPreviewDto, ApiError};
use async_trait::async_trait;
use axon_core::preview_generation::PreviewUrl;
use uuid::Uuid;

#[async_trait]
pub trait PreviewGenerator: Send + Sync {
    async fn generate(
        &self,
        account_id: Uuid,
        url: PreviewUrl,
    ) -> Result<GeneratedPreviewDto, ApiError>;
}
pub struct DisabledPreviewGenerator;
#[async_trait]
impl PreviewGenerator for DisabledPreviewGenerator {
    async fn generate(&self, _: Uuid, _: PreviewUrl) -> Result<GeneratedPreviewDto, ApiError> {
        Ok(GeneratedPreviewDto::fallback(
            crate::dto::PreviewGenerationStatus::Unsupported,
        ))
    }
}
