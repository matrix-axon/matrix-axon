//! Composition-root adapter: puts `axon-media`'s bounded disk [`MediaCache`] in
//! front of `axon-sync`'s [`SdkMediaProxy`] fetcher and binds the pair to
//! `axon-api`'s [`MediaProxy`] port.
//!
//! This binary is the one place that knows all three crates, so the wiring and
//! the error translation live here. `axon-api`, `axon-media`, and `axon-sync`
//! never depend on each other's ports.

use std::time::Duration;

use async_trait::async_trait;
use axon_api::{GrowingResource, GrowingSource, MediaBody, MediaError, MediaProxy, MediaResource};
use axon_media::{FetchError, GrowingMedia, MediaCache, MediaCacheError};
use axon_sync::SdkMediaProxy;
use bytes::Bytes;
use futures_util::stream::BoxStream;
use uuid::Uuid;

/// Serves media through the on-disk LRU cache, downloading through the SDK
/// fetcher on a miss.
pub struct CachingMediaProxy {
    cache: MediaCache,
    fetcher: SdkMediaProxy,
}

impl CachingMediaProxy {
    pub fn new(cache: MediaCache, fetcher: SdkMediaProxy) -> Self {
        Self { cache, fetcher }
    }
}

/// Convert the cache's SDK-free resource onto the API-layer type. Shared by
/// [`MediaProxy::get_media`] and [`MediaProxy::get_thumbnail`], which differ
/// only in which cache entry they resolve.
fn to_api_resource(resource: axon_media::MediaResource) -> MediaResource {
    MediaResource {
        file: resource.file,
        len: resource.len,
        etag: resource.etag,
    }
}

/// A streamed download still running in the cache, read through the API's
/// [`GrowingSource`] port (ADR 0139).
struct CacheGrowing(GrowingMedia);

#[async_trait]
impl GrowingSource for CacheGrowing {
    fn known_len(&self) -> Option<u64> {
        self.0.known_len()
    }

    async fn wait_for_len(&mut self, wait: Duration) -> Result<Option<u64>, MediaError> {
        self.0.wait_for_len(wait).await.map_err(map_err)
    }

    fn into_body(
        self: Box<Self>,
        start: u64,
        end: Option<u64>,
    ) -> BoxStream<'static, std::io::Result<Bytes>> {
        self.0.into_body(start, end)
    }
}

/// Map a cache/fetch error onto the API-layer media error. The fetcher's
/// `FetchError` was already collapsed from `axon-sync`'s `GatewayError`; a
/// too-large object is reported as an upstream failure (we won't proxy it), and
/// a genuine local disk failure is an internal error.
fn map_err(err: MediaCacheError) -> MediaError {
    match err {
        MediaCacheError::Fetch(fetch) => match fetch {
            FetchError::Internal(msg) => MediaError::Internal(msg),
            FetchError::AccountNotFound(msg) => MediaError::AccountNotFound(msg),
            FetchError::NotFound(msg) => MediaError::NotFound(msg),
            FetchError::Invalid(msg) => MediaError::Invalid(msg),
            FetchError::Forbidden(msg) => MediaError::Forbidden(msg),
            FetchError::NotConnected(msg) => MediaError::NotConnected(msg),
            FetchError::Upstream(msg) => MediaError::Upstream(msg),
            FetchError::Undecryptable(msg) => MediaError::Undecryptable(msg),
        },
        MediaCacheError::TooLarge { len, cap } => MediaError::TooLarge(format!(
            "media object too large to proxy ({len} bytes; the limit is {cap} bytes)"
        )),
        MediaCacheError::Io(e) => MediaError::Internal(e.to_string()),
        MediaCacheError::Busy(msg) => MediaError::Busy(msg),
    }
}

#[async_trait]
impl MediaProxy for CachingMediaProxy {
    async fn get_media(
        &self,
        account_id: Uuid,
        mxc_url: &str,
        encrypted_file: Option<serde_json::Value>,
    ) -> Result<MediaResource, MediaError> {
        self.cache
            .get_or_fetch(account_id, mxc_url, encrypted_file, &self.fetcher)
            .await
            .map(to_api_resource)
            .map_err(map_err)
    }

    fn etag(&self, mxc_url: &str, encrypted_file: Option<&serde_json::Value>) -> String {
        axon_media::etag_for_media(mxc_url, encrypted_file)
    }

    async fn get_media_stream(
        &self,
        account_id: Uuid,
        mxc_url: &str,
        encrypted_file: Option<serde_json::Value>,
    ) -> Result<MediaBody, MediaError> {
        let body = self
            .cache
            .get_or_stream(account_id, mxc_url, encrypted_file, &self.fetcher)
            .await
            .map_err(map_err)?;
        Ok(match body {
            axon_media::MediaBody::Complete(resource) => {
                MediaBody::Complete(to_api_resource(resource))
            }
            axon_media::MediaBody::Growing(media) => MediaBody::Growing(GrowingResource {
                etag: media.etag.clone(),
                source: Box::new(CacheGrowing(media)),
            }),
        })
    }

    async fn get_thumbnail(
        &self,
        account_id: Uuid,
        mxc_url: &str,
        spec: axon_core::media::ThumbnailSpec,
    ) -> Result<MediaResource, MediaError> {
        self.cache
            .get_or_fetch_thumbnail(account_id, mxc_url, spec, &self.fetcher)
            .await
            .map(to_api_resource)
            .map_err(map_err)
    }

    fn etag_thumbnail(&self, mxc_url: &str, spec: axon_core::media::ThumbnailSpec) -> String {
        axon_media::etag_for_thumbnail(mxc_url, spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    #[tokio::test]
    async fn unexpected_fetch_adapter_error_is_a_sanitized_500() {
        let error = map_err(MediaCacheError::Fetch(FetchError::Internal(
            "private adapter detail".into(),
        )));
        let response = axon_api::ApiError::from(error).into_response();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("private adapter detail"));
    }
}
