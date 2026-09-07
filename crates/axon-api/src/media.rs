//! The media-proxy port the media handler depends on.
//!
//! `axon-api` defines this trait — the capability it *needs* — rather than
//! depending on whatever provides it. The real implementation lives in
//! `axon-sync` (via the SDK client's authenticated download), adapted onto this
//! port by `axon-server`. So this crate stays free of `axon-sync` and
//! `matrix-sdk`: handlers speak only [`MediaProxy`] and plain types.

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

/// What can go wrong fetching media. Deliberately small and HTTP-shaped: the
/// adapter that implements [`MediaProxy`] collapses its richer backend error
/// into one of these so the handler maps a stable set of statuses.
#[derive(Debug)]
pub enum MediaError {
    /// The account does not exist or is not active. → `404`.
    AccountNotFound(String),
    /// The media was not found on the homeserver. → `404`.
    NotFound(String),
    /// The MXC URI is syntactically invalid. → `400`.
    Invalid(String),
    /// The homeserver refused the request (e.g. `M_FORBIDDEN`). → `403`.
    Forbidden(String),
    /// The account is not yet reachable; caller should retry. → `503`.
    NotConnected(String),
    /// The homeserver was unreachable or failed the request. → `502`.
    Upstream(String),
    /// The object exceeds a configured size limit and is refused. A terminal
    /// condition (retrying will not help). → `413`.
    TooLarge(String),
    /// An internal failure in the proxy/cache (e.g. local disk error). → `500`.
    /// The caller is expected to log the detail (with `account_id`) before
    /// converting, since the `500` body is generic.
    Internal(String),
    /// The object was downloaded intact but could not be decrypted — a failed
    /// `hashes.sha256` check, or a malformed `content.file` descriptor. → `422`.
    ///
    /// Separate from [`Upstream`](Self::Upstream), which it used to be folded
    /// into, for two reasons: the homeserver did nothing wrong, and this is
    /// *terminal* — a retry re-fetches the same ciphertext and fails
    /// identically — so a client that retries a `502` must not retry this
    /// (issue #359).
    Undecryptable(String),
}

impl From<MediaError> for crate::response::ApiError {
    fn from(err: MediaError) -> Self {
        match err {
            MediaError::AccountNotFound(msg) | MediaError::NotFound(msg) => Self::not_found(msg),
            MediaError::Invalid(msg) => Self::bad_request(msg),
            MediaError::Forbidden(msg) => Self::forbidden(msg),
            MediaError::NotConnected(msg) => Self::service_unavailable(msg),
            MediaError::Upstream(msg) => Self::bad_gateway(msg),
            MediaError::TooLarge(msg) => Self::payload_too_large(msg),
            MediaError::Internal(_) => Self::internal(),
            MediaError::Undecryptable(msg) => Self::media_undecryptable(msg),
        }
    }
}

/// A resolved media object, ready to serve. Holds an **open** file handle
/// (the cache opens it) whose backing bytes survive a concurrent cache
/// eviction/purge, so the handler can stream ranges from it without racing the
/// cache. The MIME type is *not* carried here — the media handler derives it
/// from the Matrix event content (the source of truth for `info.mimetype`),
/// since the SDK download does not surface a `Content-Type`.
pub struct MediaResource {
    /// Open, read-only handle positioned at the start of the media bytes.
    pub file: tokio::fs::File,
    /// Total length in bytes.
    pub len: u64,
    /// A stable, content-addressed entity tag for `ETag` / `If-None-Match`.
    /// MXC content is immutable, so this never changes for a given URI.
    pub etag: String,
}

/// Fetches media from a homeserver on behalf of an account (caching it on
/// disk). Implemented outside this crate; held in [`AppState`](crate::AppState)
/// as `Arc<dyn MediaProxy>`.
#[async_trait]
pub trait MediaProxy: Send + Sync {
    /// Resolve the media identified by `mxc_url` (`mxc://server/media_id`) using
    /// the given account's credentials, returning an open handle to the bytes.
    /// `encrypted_file` is the matching `content.file` or
    /// `content.info.thumbnail_file` JSON object from the Matrix event when the
    /// media is encrypted; the implementation uses it to decrypt after
    /// downloading. Pass `None` for plain (unencrypted) media.
    async fn get_media(
        &self,
        account_id: Uuid,
        mxc_url: &str,
        encrypted_file: Option<Value>,
    ) -> Result<MediaResource, MediaError>;

    /// The stable entity tag for `mxc_url`, computed **without** any download.
    /// MXC content is immutable, so this is content-addressed (a hash of the
    /// URI). The handler uses it to answer a conditional GET (`If-None-Match`)
    /// with `304` before triggering a fetch on a cache miss.
    fn etag(&self, mxc_url: &str) -> String;

    /// Resolve a homeserver-generated thumbnail of `mxc_url` at `spec`'s
    /// dimensions/method, using the given account's credentials.
    ///
    /// **Plain media only** — the thumbnail route handler has already
    /// rejected encrypted media with a `400` before this is ever invoked (a
    /// homeserver never sees encrypted-media plaintext, so it cannot
    /// thumbnail it), so unlike [`get_media`](Self::get_media) there is no
    /// `encrypted_file` parameter.
    async fn get_thumbnail(
        &self,
        account_id: Uuid,
        mxc_url: &str,
        spec: axon_core::media::ThumbnailSpec,
    ) -> Result<MediaResource, MediaError>;

    /// The thumbnail-aware sibling of [`etag`](Self::etag) — content-addressed
    /// on `(mxc_url, spec)`, computed without any download.
    fn etag_thumbnail(&self, mxc_url: &str, spec: axon_core::media::ThumbnailSpec) -> String;
}
