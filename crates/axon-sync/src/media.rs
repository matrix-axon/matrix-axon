//! Authenticated media download via the matrix-rust-sdk client.
//!
//! [`SdkMediaProxy`] is the [`MediaFetcher`] the bounded on-disk media cache
//! (`axon-media`) calls on a cache miss. It resolves the account's live SDK
//! client through the [`ClientManager`] (connecting if needed), then uses the
//! SDK's built-in media API — which carries the account's Bearer token
//! automatically — to download (and, for encrypted attachments, decrypt) MXC
//! content, returning the plaintext bytes.
//!
//! `axon-server` composes this fetcher behind the `axon-media` cache and adapts
//! the pair onto the `axon-api` `MediaProxy` port, so neither `axon-api` nor
//! `axon-media` depends on `matrix-sdk`.

use std::time::Duration;

use async_trait::async_trait;
use axon_core::media::{ThumbnailMethod, ThumbnailSpec};
use axon_media::{FetchError, MediaFetcher};
use matrix_sdk::media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings};
use matrix_sdk::ruma::{
    api::client::media::get_content_thumbnail::v3::Method as SdkThumbnailMethod,
    api::error::ErrorKind,
    events::room::{EncryptedFile, MediaSource},
};
use matrix_sdk::Error as SdkError;
use uuid::Uuid;

use crate::error::GatewayError;
use crate::manager::ClientManager;

/// Downloads MXC media through an account's live SDK client, on behalf of the
/// `axon-media` cache.
///
/// Cheap to [`Clone`] — holds only a [`ClientManager`] and the fetch timeout.
#[derive(Clone)]
pub struct SdkMediaProxy {
    manager: ClientManager,
    fetch_timeout: Duration,
}

impl SdkMediaProxy {
    pub fn new(manager: ClientManager, fetch_timeout: Duration) -> Self {
        Self {
            manager,
            fetch_timeout,
        }
    }

    /// Download `mxc_url` using `account_id`'s authenticated SDK client, bounded
    /// by the configured fetch timeout so a hung homeserver can't await forever.
    async fn download(
        &self,
        account_id: Uuid,
        mxc_url: &str,
        encrypted_file: Option<serde_json::Value>,
    ) -> Result<Vec<u8>, GatewayError> {
        // Validate the MXC URI before doing any network work.
        axon_media::parse_mxc(mxc_url)
            .ok_or_else(|| GatewayError::Invalid(format!("invalid MXC URI: {mxc_url}")))?;

        let client = self.manager.get_or_connect(account_id).await?;

        // When the event carries a `content.file` object the media is encrypted;
        // deserialize it into ruma's `EncryptedFile` so the SDK can download and
        // decrypt in one step. Fall back to plain download otherwise.
        let (source, encrypted) = if let Some(file_json) = encrypted_file {
            let enc: EncryptedFile = serde_json::from_value(file_json)
                .map_err(|e| GatewayError::Invalid(format!("invalid encrypted file: {e}")))?;
            (MediaSource::Encrypted(Box::new(enc)), true)
        } else {
            (MediaSource::Plain(mxc_url.into()), false)
        };

        let request = MediaRequestParameters {
            source,
            format: MediaFormat::File,
        };

        // `false` disables the SDK's own media cache: axon-media is the cache of
        // record, and letting the SDK also cache would double disk use with no
        // benefit (and outside our LRU accounting).
        let media = client.media();
        let download = media.get_media_content(&request, false);
        let data = tokio::time::timeout(self.fetch_timeout, download)
            .await
            .map_err(|_| {
                GatewayError::Upstream(format!(
                    "media download timed out after {}s",
                    self.fetch_timeout.as_secs()
                ))
            })?
            .map_err(|error| classify_download_error(mxc_url, encrypted, error))?;

        Ok(data)
    }

    /// Request a homeserver-generated thumbnail of `mxc_url` at `spec`'s
    /// dimensions/method, bounded by the same fetch timeout as [`download`].
    ///
    /// **Plain media only.** `Media::get_media_content` (matrix-sdk 0.18.0)
    /// only honors `MediaFormat::Thumbnail` when `request.source` is
    /// `MediaSource::Plain` — the `Encrypted` arm always downloads and
    /// decrypts the full ciphertext regardless of `format`, since a
    /// homeserver never sees encrypted-media plaintext to thumbnail. The API
    /// layer rejects encrypted media with a `400` before this is ever called,
    /// so there is no `encrypted_file` parameter here.
    async fn download_thumbnail(
        &self,
        account_id: Uuid,
        mxc_url: &str,
        spec: ThumbnailSpec,
    ) -> Result<Vec<u8>, GatewayError> {
        axon_media::parse_mxc(mxc_url)
            .ok_or_else(|| GatewayError::Invalid(format!("invalid MXC URI: {mxc_url}")))?;

        let client = self.manager.get_or_connect(account_id).await?;

        let request = MediaRequestParameters {
            source: MediaSource::Plain(mxc_url.into()),
            format: MediaFormat::Thumbnail(MediaThumbnailSettings::with_method(
                to_sdk_method(spec.method),
                spec.width.into(),
                spec.height.into(),
            )),
        };

        let media = client.media();
        let download = media.get_media_content(&request, false);
        let data = tokio::time::timeout(self.fetch_timeout, download)
            .await
            .map_err(|_| {
                GatewayError::Upstream(format!(
                    "media thumbnail download timed out after {}s",
                    self.fetch_timeout.as_secs()
                ))
            })?
            .map_err(|error| classify_download_error(mxc_url, false, error))?;

        Ok(data)
    }
}

/// Map the shared, SDK-free [`ThumbnailMethod`] onto the SDK/ruma `Method`
/// that [`MediaThumbnailSettings`] needs.
fn to_sdk_method(method: ThumbnailMethod) -> SdkThumbnailMethod {
    match method {
        ThumbnailMethod::Crop => SdkThumbnailMethod::Crop,
        ThumbnailMethod::Scale => SdkThumbnailMethod::Scale,
    }
}

#[async_trait]
impl MediaFetcher for SdkMediaProxy {
    async fn fetch(
        &self,
        account_id: Uuid,
        mxc_url: &str,
        encrypted_file: Option<serde_json::Value>,
    ) -> Result<Vec<u8>, FetchError> {
        self.download(account_id, mxc_url, encrypted_file)
            .await
            .map_err(gateway_to_fetch)
    }

    async fn fetch_thumbnail(
        &self,
        account_id: Uuid,
        mxc_url: &str,
        spec: ThumbnailSpec,
    ) -> Result<Vec<u8>, FetchError> {
        self.download_thumbnail(account_id, mxc_url, spec)
            .await
            .map_err(gateway_to_fetch)
    }
}

/// Sort a failed download into "the homeserver let us down" and "the bytes are
/// there but will not decrypt" — a distinction the API boundary needs, because
/// only the first is worth retrying (issue #359).
///
/// `encrypted` is what makes this reliable without matching on error strings.
/// On the encrypted path `get_media_content` (matrix-sdk 0.18) does exactly two
/// fallible things after the HTTP fetch: it builds an `AttachmentDecryptor`,
/// whose failures are `DecryptorError` (bad base64, missing hash, unknown `v`),
/// and it `read_to_end`s through it, whose failures are the decryptor's own
/// `io::Error` — the `"Hash mismatch while decrypting"` that a wrong key or
/// corrupted ciphertext produces. Transport failures surface as HTTP variants,
/// not `Io`. On the plain path there is no decryptor at all, so neither variant
/// can mean decryption and both stay `Upstream`.
///
/// **This reads two undocumented upstream details, so name them precisely.**
/// The workspace takes `matrix-sdk = "0.18"` (caret), meaning a patch release
/// can change either without a compile error here — and a misclassification is
/// silent in both directions: a network blip reported as a permanent `422`, or
/// a real decryption failure reported as retryable. The two lines to re-read on
/// any bump, verified against 0.18.0:
///
/// - `matrix-sdk-0.18.0/src/media.rs`, `Media::get_media_content` — the
///   `#[cfg(feature = "e2e-encryption")]` block that wraps the fetched bytes in
///   `AttachmentDecryptor::new(...)?` and then `reader.read_to_end(&mut …)?`.
///   Those two `?`s are the entire fallible surface this function reasons about.
/// - `matrix-sdk-crypto-0.18.0/src/file_encryption/attachments.rs`, the
///   `impl Read for AttachmentDecryptor` — `IoError::other("Hash mismatch while
///   decrypting")` on the final zero-length read. This is why the *common* case
///   is `Io` and not `DecryptorError`, which is the counter-intuitive half.
///
/// Tracked in issue #376, which also weighs an integration test through the
/// real decryptor — the only check that would actually fail if this changed.
fn classify_download_error(mxc_url: &str, encrypted: bool, error: SdkError) -> GatewayError {
    if error.client_api_error_kind() == Some(&ErrorKind::NotFound) {
        return GatewayError::MediaNotFound(mxc_url.to_owned());
    }
    if encrypted && matches!(error, SdkError::DecryptorError(_) | SdkError::Io(_)) {
        return GatewayError::Undecryptable(error.to_string());
    }
    GatewayError::Upstream(error.to_string())
}

/// Collapse the sync-layer [`GatewayError`] onto the cache-neutral
/// [`FetchError`] the `axon-media` cache passes back to the API adapter.
fn gateway_to_fetch(err: GatewayError) -> FetchError {
    match err {
        GatewayError::UnknownAccount(id) => {
            FetchError::AccountNotFound(format!("no such account: {id}"))
        }
        GatewayError::AccountNotActive(id) => {
            FetchError::AccountNotFound(format!("account not active: {id}"))
        }
        GatewayError::Invalid(msg) => FetchError::Invalid(msg),
        GatewayError::MediaNotFound(msg) => FetchError::NotFound(format!("media not found: {msg}")),
        GatewayError::RoomNotFound(msg) => FetchError::NotFound(format!("room not found: {msg}")),
        GatewayError::Forbidden(msg) => FetchError::Forbidden(msg),
        GatewayError::NotConnected(msg) => FetchError::NotConnected(msg),
        GatewayError::Upstream(msg) => FetchError::Upstream(msg),
        GatewayError::Undecryptable(msg) => FetchError::Undecryptable(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::encryption::DecryptorError;

    const MXC: &str = "mxc://example.org/abc";

    #[test]
    fn hash_mismatch_on_the_encrypted_path_is_undecryptable() {
        // What a wrong key or corrupted ciphertext actually produces: the
        // `AttachmentDecryptor`'s `Read` impl returns this from `read_to_end`,
        // and matrix-sdk wraps it as `Error::Io` — not as a `DecryptorError`,
        // which is the subtlety this classification turns on.
        let err = SdkError::Io(std::io::Error::other("Hash mismatch while decrypting"));
        assert!(matches!(
            classify_download_error(MXC, true, err),
            GatewayError::Undecryptable(_)
        ));
    }

    #[test]
    fn malformed_descriptor_is_undecryptable() {
        let err = SdkError::DecryptorError(DecryptorError::MissingHash);
        assert!(matches!(
            classify_download_error(MXC, true, err),
            GatewayError::Undecryptable(_)
        ));
    }

    #[test]
    fn the_same_io_error_on_the_plain_path_stays_upstream() {
        // No decryptor runs for plaintext media, so an `Io` error there cannot
        // mean decryption — reporting it as `422 media_undecryptable` would
        // tell a client the failure is terminal when it may well be transient.
        let err = SdkError::Io(std::io::Error::other("connection reset"));
        assert!(matches!(
            classify_download_error(MXC, false, err),
            GatewayError::Upstream(_)
        ));
    }

    #[test]
    fn a_decryptor_error_on_the_plain_path_stays_upstream() {
        let err = SdkError::DecryptorError(DecryptorError::UnknownVersion);
        assert!(matches!(
            classify_download_error(MXC, false, err),
            GatewayError::Upstream(_)
        ));
    }
}
