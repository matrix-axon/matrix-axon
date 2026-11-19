//! Streamed media download from the homeserver (ADR 0139 section 2).
//!
//! matrix-sdk's `Media::get_media_content` returns the whole object as a
//! `Vec<u8>`, so video and audio could start playing only after their last
//! byte arrived. This module makes the same authenticated download request
//! with the SDK client's own `reqwest::Client`, homeserver URL and access
//! token, and hands the body on chunk by chunk, decrypting an encrypted
//! attachment as it goes.
//!
//! It sets no deadline of its own. The `axon-media` cache bounds the wait for
//! the response headers and every body read, and checks the body's length.

use std::time::Duration;

use axon_media::{FetchError, MediaStream};
use bytes::Bytes;
use ctr::cipher::{KeyIvInit, StreamCipher};
use futures_util::stream::{self, BoxStream, StreamExt};
use matrix_sdk::ruma::api::client::authenticated_media::get_content::v1::Request as AuthenticatedGetContent;
use matrix_sdk::ruma::api::Metadata;
use matrix_sdk::ruma::events::room::{
    EncryptedFile, EncryptedFileHash, EncryptedFileHashAlgorithm, EncryptedFileInfo,
};
use matrix_sdk::{Client, RefreshTokenError};
use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING, RETRY_AFTER};
use reqwest::StatusCode;
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use crate::error::GatewayError;

type Aes256Ctr = ctr::Ctr128BE<aes::Aes256>;

/// The most of an error response's body read to find its `errcode`.
const MAX_ERROR_BYTES: usize = 64 * 1024;

/// How many times the download request is sent when the homeserver answers
/// `429` or a `5xx`, as the SDK retries such transient errors for its own
/// buffered download. The caller's `fetch_timeout_secs` deadline bounds the
/// whole sequence, so a homeserver asking for a long wait gets a `503`.
const MAX_ATTEMPTS: u32 = 4;

/// The first retry's delay when the homeserver names none; each later retry
/// doubles it. The SDK's backoff starts at the same 500 ms.
#[cfg(not(test))]
const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
#[cfg(test)]
const RETRY_BASE_DELAY: Duration = Duration::from_millis(10);

/// The longest one retry waits, even when the homeserver asks for longer.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(10);

/// Decrypts an encrypted attachment chunk by chunk: AES-256-CTR over the
/// ciphertext, with a SHA-256 over the ciphertext checked after the last
/// chunk. The same construction as matrix-sdk-crypto's
/// `AttachmentDecryptor`, which only reads a whole object.
pub(crate) struct StreamDecryptor {
    aes: Aes256Ctr,
    sha: Sha256,
    expected_hash: [u8; 32],
}

impl StreamDecryptor {
    /// Make the checks `AttachmentDecryptor::new` makes — a `v2` descriptor
    /// with a SHA-256 hash — before any network work. Failing either is
    /// `Undecryptable`, as it is on the buffered path. A wrong-length key or
    /// IV never gets here: ruma rejects it while parsing the descriptor.
    pub(crate) fn new(file: &EncryptedFile) -> Result<Self, GatewayError> {
        let EncryptedFileInfo::V2(info) = &file.info else {
            return Err(GatewayError::Undecryptable(
                "unknown version for the encrypted attachment".to_owned(),
            ));
        };
        let Some(EncryptedFileHash::Sha256(hash)) =
            file.hashes.get(&EncryptedFileHashAlgorithm::Sha256)
        else {
            return Err(GatewayError::Undecryptable(
                "the encryption info is missing a hash".to_owned(),
            ));
        };
        Ok(Self {
            aes: Aes256Ctr::new(info.k.as_inner().into(), info.iv.as_inner().into()),
            sha: Sha256::new(),
            expected_hash: *hash.as_inner(),
        })
    }

    fn decrypt(&mut self, chunk: &[u8]) -> Bytes {
        self.sha.update(chunk);
        let mut plaintext = chunk.to_vec();
        self.aes.apply_keystream(&mut plaintext);
        Bytes::from(plaintext)
    }

    fn verify(self) -> Result<(), FetchError> {
        if self.sha.finalize().as_slice() == self.expected_hash {
            Ok(())
        } else {
            Err(FetchError::Undecryptable(
                "hash mismatch while decrypting".to_owned(),
            ))
        }
    }
}

/// Wrap a ciphertext stream so it yields plaintext. Its last item is an
/// `Undecryptable` error when the ciphertext hash does not match; a clean end
/// means it did.
///
/// A body that ends cleanly before its declared length `len` is cut short,
/// not tampered with, so it ends with a transient `Upstream` error and gets no
/// hash verdict. A hash verdict would be `Undecryptable`, which the cache
/// remembers and refuses retries of for 10 minutes. An unknown-length body
/// can't be checked this way, but a truncated chunked or gzip body is already
/// a transport error.
pub(crate) fn decrypting(
    body: BoxStream<'static, Result<Bytes, FetchError>>,
    decryptor: StreamDecryptor,
    len: Option<u64>,
) -> BoxStream<'static, Result<Bytes, FetchError>> {
    stream::unfold(Some((body, decryptor, 0u64)), move |state| async move {
        let (mut body, mut decryptor, seen) = state?;
        match body.next().await {
            Some(Ok(chunk)) => {
                let seen = seen + chunk.len() as u64;
                let plaintext = decryptor.decrypt(&chunk);
                Some((Ok(plaintext), Some((body, decryptor, seen))))
            }
            Some(Err(error)) => Some((Err(error), None)),
            None => match len {
                Some(len) if seen != len => Some((
                    Err(FetchError::Upstream(format!(
                        "media body ended at {seen} of its {len} bytes"
                    ))),
                    None,
                )),
                _ => decryptor.verify().err().map(|error| (Err(error), None)),
            },
        }
    })
    .boxed()
}

/// The URL to download `server_name`/`media_id` from. The authenticated
/// endpoint when the homeserver supports it, the legacy one otherwise.
pub(crate) fn download_url(
    homeserver: &Url,
    server_name: &str,
    media_id: &str,
    authenticated: bool,
) -> Option<Url> {
    let mut url = homeserver.clone();
    {
        let mut segments = url.path_segments_mut().ok()?;
        segments.pop_if_empty();
        if authenticated {
            segments.extend(["_matrix", "client", "v1", "media", "download"]);
        } else {
            segments.extend(["_matrix", "media", "v3", "download"]);
        }
        segments.extend([server_name, media_id]);
    }
    Some(url)
}

/// Open `account_id`'s streamed download of `mxc_url`, decrypting with
/// `decryptor` when the media is encrypted. Returns once the response headers
/// have arrived.
pub(crate) async fn open(
    client: &Client,
    account_id: Uuid,
    mxc_url: &str,
    decryptor: Option<StreamDecryptor>,
) -> Result<MediaStream, GatewayError> {
    let (server_name, media_id) = axon_media::parse_mxc(mxc_url)
        .ok_or_else(|| GatewayError::Invalid(format!("invalid MXC URI: {mxc_url}")))?;

    // The same choice the SDK makes for its own download.
    let supported = client
        .supported_versions()
        .await
        .map_err(|e| GatewayError::Upstream(format!("homeserver versions unavailable: {e}")))?;
    let authenticated = AuthenticatedGetContent::PATH_BUILDER.is_supported(&supported);
    let url = download_url(&client.homeserver(), server_name, media_id, authenticated)
        .ok_or_else(|| GatewayError::Invalid("homeserver URL cannot carry a path".to_owned()))?;

    let mut refreshed = false;
    let mut attempt = 1;
    let response = loop {
        let response = send(client, url.clone(), authenticated).await?;
        let status = response.status();
        if status.is_success() {
            break response;
        }
        let error = ErrorAnswer::read(response).await;

        if status == StatusCode::UNAUTHORIZED
            && error.errcode.as_deref() == Some("M_UNKNOWN_TOKEN")
            && !refreshed
        {
            // A MAS-issued token (ADR 0097) may simply have expired: refresh
            // once and send again.
            refreshed = true;
            match client.refresh_access_token().await {
                Ok(()) => continue,
                // A session with no refresh token cannot recover here. Sync
                // meets the same answer and reports the expired session
                // through the SDK; this request fails as the buffered
                // download's would.
                Err(RefreshTokenError::RefreshTokenRequired) => {
                    return Err(GatewayError::Upstream(
                        "homeserver rejected the access token (M_UNKNOWN_TOKEN), \
                         and the session has no refresh token"
                            .to_owned(),
                    ));
                }
                Err(e) => {
                    return Err(GatewayError::Upstream(format!(
                        "access token refresh failed: {e}"
                    )));
                }
            }
        }

        let transient = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
        if transient && attempt < MAX_ATTEMPTS {
            let backoff = RETRY_BASE_DELAY * 2u32.pow(attempt - 1);
            let delay = error.retry_after.unwrap_or(backoff).min(MAX_RETRY_DELAY);
            tracing::debug!(
                %account_id,
                mxc = %mxc_url,
                %status,
                attempt,
                delay_ms = delay.as_millis() as u64,
                "retrying a transient media download error",
            );
            tokio::time::sleep(delay).await;
            attempt += 1;
            continue;
        }

        if status == StatusCode::NOT_FOUND || error.errcode.as_deref() == Some("M_NOT_FOUND") {
            return Err(GatewayError::MediaNotFound(mxc_url.to_owned()));
        }
        return Err(GatewayError::Upstream(format!(
            "homeserver answered the media download with {status}{}",
            error
                .errcode
                .map(|code| format!(" ({code})"))
                .unwrap_or_default()
        )));
    };

    // matrix-sdk enables reqwest's `gzip` feature, so a gzip body is decoded
    // transparently: reqwest drops `Content-Encoding` and `Content-Length`,
    // and the plaintext arrives as an unknown-length stream. Any encoding
    // reqwest does not decode would otherwise be cached still encoded, under a
    // length that is not the media's, so refuse it.
    if let Some(encoding) = response.headers().get(CONTENT_ENCODING) {
        if !encoding.as_bytes().eq_ignore_ascii_case(b"identity") {
            return Err(GatewayError::Upstream(format!(
                "homeserver sent the media with Content-Encoding {encoding:?}"
            )));
        }
    }

    let len = response.content_length();
    if len.is_none() {
        // ADR 0139 asks whether Synapse ever omits it; this is the record.
        tracing::info!(
            %account_id,
            mxc = %mxc_url,
            "homeserver sent media without a Content-Length"
        );
    }

    let body = body_stream(response);
    let body = match decryptor {
        Some(decryptor) => decrypting(body, decryptor, len),
        None => body,
    };
    Ok(MediaStream { len, body })
}

/// One GET of the download URL with the current access token.
async fn send(
    client: &Client,
    url: Url,
    authenticated: bool,
) -> Result<reqwest::Response, GatewayError> {
    let mut request = client
        .http_client()
        .get(url)
        .header(ACCEPT_ENCODING, "identity")
        // The SDK's client carries a 30 s whole-request timeout, which reqwest
        // applies to the body too. A download has no total limit (the cache
        // bounds stalls instead), as in the SDK's own media fetch.
        .timeout(Duration::MAX);
    if authenticated {
        let token = client.access_token().ok_or_else(|| {
            GatewayError::NotConnected("the account has no access token".to_owned())
        })?;
        request = request.bearer_auth(token);
    }
    request
        .send()
        .await
        .map_err(|e| GatewayError::Upstream(format!("media download request failed: {e}")))
}

/// What an error response says about itself.
struct ErrorAnswer {
    /// The Matrix `errcode`, if the body is a Matrix error.
    errcode: Option<String>,
    /// How long the homeserver asks the client to wait: the body's
    /// `retry_after_ms`, else a `Retry-After` header in seconds.
    retry_after: Option<Duration>,
}

impl ErrorAnswer {
    /// Read the answer from a bounded body. An unreadable or oversized body
    /// names nothing.
    async fn read(mut response: reqwest::Response) -> Self {
        let header_wait = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let mut body = Vec::new();
        let mut oversized = false;
        while let Ok(Some(chunk)) = response.chunk().await {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_ERROR_BYTES {
                oversized = true;
                break;
            }
        }
        let json = (!oversized)
            .then(|| serde_json::from_slice::<Value>(&body).ok())
            .flatten();
        let field = |name: &str| json.as_ref().and_then(|json| json.get(name).cloned());
        Self {
            errcode: field("errcode").and_then(|code| code.as_str().map(str::to_owned)),
            retry_after: field("retry_after_ms")
                .and_then(|ms| ms.as_u64())
                .map(Duration::from_millis)
                .or(header_wait),
        }
    }
}

/// The response body as a stream of chunks. A transport error ends it with
/// that error; hyper reports a connection dropped mid-body as one.
fn body_stream(response: reqwest::Response) -> BoxStream<'static, Result<Bytes, FetchError>> {
    stream::unfold(Some(response), |response| async move {
        let mut response = response?;
        match response.chunk().await {
            Ok(Some(chunk)) => Some((Ok(chunk), Some(response))),
            Ok(None) => None,
            Err(error) => Some((
                Err(FetchError::Upstream(format!(
                    "media download failed: {error}"
                ))),
                None,
            )),
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use matrix_sdk::ruma::api::MatrixVersion;
    use matrix_sdk::ruma::events::room::{EncryptedFileHashes, V2EncryptedFileInfo};
    use matrix_sdk::ruma::mxc_uri;
    use matrix_sdk::ruma::serde::Base64;

    const KEY: [u8; 32] = [7; 32];
    const IV: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 0, 0, 0, 0, 0, 0, 0, 0];

    fn encrypt(plaintext: &[u8]) -> Vec<u8> {
        let mut ciphertext = plaintext.to_vec();
        Aes256Ctr::new((&KEY).into(), (&IV).into()).apply_keystream(&mut ciphertext);
        ciphertext
    }

    fn descriptor(hash: [u8; 32]) -> EncryptedFile {
        let mut hashes = EncryptedFileHashes::new();
        hashes.insert(EncryptedFileHash::Sha256(Base64::new(hash)));
        EncryptedFile::new(
            mxc_uri!("mxc://example.org/abc").to_owned(),
            EncryptedFileInfo::V2(V2EncryptedFileInfo::new(Base64::new(KEY), Base64::new(IV))),
            hashes,
        )
    }

    fn chunks(data: &[u8], size: usize) -> BoxStream<'static, Result<Bytes, FetchError>> {
        let chunks: Vec<Result<Bytes, FetchError>> = data
            .chunks(size)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        stream::iter(chunks).boxed()
    }

    async fn drain(
        mut body: BoxStream<'static, Result<Bytes, FetchError>>,
    ) -> (Vec<u8>, Option<FetchError>) {
        let mut out = Vec::new();
        while let Some(item) = body.next().await {
            match item {
                Ok(chunk) => out.extend_from_slice(&chunk),
                Err(error) => return (out, Some(error)),
            }
        }
        (out, None)
    }

    #[tokio::test]
    async fn decrypts_what_the_sdk_encrypts_across_uneven_chunks() {
        use std::io::Read;

        // Encrypt with matrix-sdk-crypto's own attachment encryptor, so this
        // checks the streamed path against the format real senders produce,
        // not against itself.
        let plaintext: Vec<u8> = (0..100_000u32).map(|i| (i % 253) as u8).collect();
        let mut source = plaintext.as_slice();
        let mut encryptor = matrix_sdk_base::crypto::AttachmentEncryptor::new(&mut source);
        let mut ciphertext = Vec::new();
        encryptor.read_to_end(&mut ciphertext).unwrap();
        let info = encryptor.finish();
        let file = EncryptedFile::new(
            mxc_uri!("mxc://example.org/abc").to_owned(),
            info.encryption_info,
            info.hashes,
        );
        let decryptor = StreamDecryptor::new(&file).unwrap();

        // 1000 is not a multiple of the 16-byte AES block, so the counter
        // must carry across chunk boundaries.
        let (out, error) = drain(decrypting(
            chunks(&ciphertext, 1000),
            decryptor,
            Some(ciphertext.len() as u64),
        ))
        .await;
        assert!(error.is_none(), "{error:?}");
        assert_eq!(out, plaintext);
    }

    #[tokio::test]
    async fn a_flipped_ciphertext_bit_fails_the_hash() {
        let ciphertext = encrypt(&[9; 10_000]);
        let hash: [u8; 32] = Sha256::digest(&ciphertext).into();
        let mut tampered = ciphertext.clone();
        tampered[5_000] ^= 1;
        let decryptor = StreamDecryptor::new(&descriptor(hash)).unwrap();
        let (_, error) = drain(decrypting(
            chunks(&tampered, 4096),
            decryptor,
            Some(tampered.len() as u64),
        ))
        .await;
        assert!(matches!(error, Some(FetchError::Undecryptable(_))));
    }

    #[tokio::test]
    async fn a_hash_mismatch_is_the_last_item_and_undecryptable() {
        let ciphertext = encrypt(&[1; 4096]);
        let decryptor = StreamDecryptor::new(&descriptor([0; 32])).unwrap();
        let (out, error) = drain(decrypting(chunks(&ciphertext, 1024), decryptor, None)).await;
        assert_eq!(
            out.len(),
            4096,
            "every chunk is passed on before the verdict"
        );
        assert!(matches!(error, Some(FetchError::Undecryptable(_))));
    }

    #[tokio::test]
    async fn a_body_cut_short_is_transient_not_undecryptable() {
        // The hash of a truncated body can never match. Reporting that as
        // `Undecryptable` would make the cache refuse retries for 10 minutes.
        let ciphertext = encrypt(&[3; 8192]);
        let hash: [u8; 32] = Sha256::digest(&ciphertext).into();
        let decryptor = StreamDecryptor::new(&descriptor(hash)).unwrap();
        let short = &ciphertext[..5000];
        let (out, error) = drain(decrypting(
            chunks(short, 1024),
            decryptor,
            Some(ciphertext.len() as u64),
        ))
        .await;
        assert_eq!(out.len(), 5000);
        assert!(
            matches!(&error, Some(FetchError::Upstream(message)) if message.contains("5000 of its 8192")),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn an_upstream_error_ends_the_stream_without_a_verdict() {
        let decryptor = StreamDecryptor::new(&descriptor([0; 32])).unwrap();
        let body: Vec<Result<Bytes, FetchError>> = vec![
            Ok(Bytes::from_static(&[1; 32])),
            Err(FetchError::Upstream("reset".into())),
        ];
        let (_, error) = drain(decrypting(stream::iter(body).boxed(), decryptor, None)).await;
        assert!(matches!(error, Some(FetchError::Upstream(_))));
    }

    #[test]
    fn a_descriptor_without_a_sha256_hash_is_undecryptable_before_any_download() {
        let mut file = descriptor([0; 32]);
        file.hashes = EncryptedFileHashes::new();
        assert!(matches!(
            StreamDecryptor::new(&file),
            Err(GatewayError::Undecryptable(_))
        ));
    }

    /// `b"hello media"`, gzip-compressed.
    const HELLO_MEDIA_GZIP: [u8; 31] = [
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0xcb, 0x48, 0xcd, 0xc9, 0xc9,
        0x57, 0xc8, 0x4d, 0x4d, 0xc9, 0x4c, 0x04, 0x00, 0xca, 0xa1, 0x11, 0x5d, 0x0b, 0x00, 0x00,
        0x00,
    ];

    /// What the mock homeserver saw of one media request.
    #[derive(Clone, Debug)]
    struct Seen {
        path: String,
        authorization: Option<String>,
        accept_encoding: Option<String>,
    }

    /// A mock homeserver for `/download/example.org/<media_id>`, where the
    /// media id picks the answer. `Bearer stale` is refused with
    /// `M_UNKNOWN_TOKEN` until a refresh hands out `Bearer fresh`.
    async fn mock_homeserver() -> (std::net::SocketAddr, Arc<Mutex<Vec<Seen>>>) {
        use axum::http::{HeaderMap, StatusCode as Status};
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::{Json, Router};

        let seen = Arc::new(Mutex::new(Vec::<Seen>::new()));
        let app = Router::new()
            .route(
                "/_matrix/client/v3/refresh",
                post(|| async {
                    Json(serde_json::json!({
                        "access_token": "fresh",
                        "refresh_token": "refresh-2",
                    }))
                }),
            )
            .fallback({
                let seen = seen.clone();
                move |uri: axum::http::Uri, headers: HeaderMap| {
                    let seen = seen.clone();
                    async move {
                        let header = |name| {
                            headers
                                .get(name)
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_owned)
                        };
                        let authorization = header("authorization");
                        // Which request this is for its path, counting from 1.
                        let nth = {
                            let mut seen = seen.lock().unwrap();
                            seen.push(Seen {
                                path: uri.path().to_owned(),
                                authorization: authorization.clone(),
                                accept_encoding: header("accept-encoding"),
                            });
                            seen.iter().filter(|s| s.path == uri.path()).count()
                        };
                        let media_id = uri.path().rsplit('/').next().unwrap_or_default().to_owned();
                        if authorization.as_deref() == Some("Bearer stale") {
                            return (
                                Status::UNAUTHORIZED,
                                Json(serde_json::json!({"errcode": "M_UNKNOWN_TOKEN"})),
                            )
                                .into_response();
                        }
                        match media_id.as_str() {
                            "plain" => b"hello media".to_vec().into_response(),
                            "missing" => (
                                Status::NOT_FOUND,
                                Json(serde_json::json!({"errcode": "M_NOT_FOUND"})),
                            )
                                .into_response(),
                            "broken" => (Status::BAD_GATEWAY, "upstream down").into_response(),
                            // Rate-limited once, naming its own wait.
                            "limited" if nth == 1 => (
                                Status::TOO_MANY_REQUESTS,
                                Json(serde_json::json!({
                                    "errcode": "M_LIMIT_EXCEEDED",
                                    "retry_after_ms": 20,
                                })),
                            )
                                .into_response(),
                            // Unavailable once, naming no wait.
                            "flaky" if nth == 1 => {
                                (Status::SERVICE_UNAVAILABLE, "restarting").into_response()
                            }
                            "limited" | "flaky" => b"hello media".to_vec().into_response(),
                            "gzipped" => {
                                ([("content-encoding", "gzip")], HELLO_MEDIA_GZIP.to_vec())
                                    .into_response()
                            }
                            "brotli" => ([("content-encoding", "br")], b"not decoded".to_vec())
                                .into_response(),
                            "slow" => {
                                // Five chunks over about a second: longer than
                                // the client's whole-request timeout.
                                let chunks = stream::iter(0..5).then(|i| async move {
                                    tokio::time::sleep(Duration::from_millis(200)).await;
                                    Ok::<_, std::io::Error>(vec![i as u8; 1024])
                                });
                                axum::body::Body::from_stream(chunks).into_response()
                            }
                            _ => Status::NOT_FOUND.into_response(),
                        }
                    }
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (address, seen)
    }

    /// An SDK client for the mock, whose own requests time out after 300 ms.
    async fn sdk_client(
        address: std::net::SocketAddr,
        version: matrix_sdk::ruma::api::MatrixVersion,
        access_token: &str,
    ) -> Client {
        sdk_client_with(address, version, access_token, Some("refresh-1")).await
    }

    async fn sdk_client_with(
        address: std::net::SocketAddr,
        version: matrix_sdk::ruma::api::MatrixVersion,
        access_token: &str,
        refresh_token: Option<&str>,
    ) -> Client {
        use matrix_sdk::authentication::matrix::MatrixSession;
        use matrix_sdk::store::RoomLoadSettings;
        use matrix_sdk::{SessionMeta, SessionTokens};

        let client = Client::builder()
            .homeserver_url(format!("http://{address}"))
            .server_versions([version])
            .request_config(
                matrix_sdk::config::RequestConfig::new()
                    .disable_retry()
                    .timeout(Duration::from_millis(300)),
            )
            .handle_refresh_tokens()
            .build()
            .await
            .unwrap();
        client
            .matrix_auth()
            .restore_session(
                MatrixSession {
                    meta: SessionMeta {
                        user_id: "@media:localhost".parse().unwrap(),
                        device_id: "MEDIA".into(),
                    },
                    tokens: SessionTokens {
                        access_token: access_token.to_owned(),
                        refresh_token: refresh_token.map(str::to_owned),
                    },
                },
                RoomLoadSettings::default(),
            )
            .await
            .unwrap();
        client
    }

    async fn open_plain(client: &Client, media_id: &str) -> Result<MediaStream, GatewayError> {
        open(
            client,
            Uuid::nil(),
            &format!("mxc://example.org/{media_id}"),
            None,
        )
        .await
    }

    #[tokio::test]
    async fn streams_from_the_authenticated_endpoint_with_identity_encoding() {
        let (address, seen) = mock_homeserver().await;
        let client = sdk_client(address, MatrixVersion::V1_11, "good").await;
        let media = open_plain(&client, "plain").await.unwrap();
        assert_eq!(media.len, Some(11));
        let (bytes, error) = drain(media.body).await;
        assert!(error.is_none());
        assert_eq!(bytes, b"hello media");

        let seen = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!(
            seen.path,
            "/_matrix/client/v1/media/download/example.org/plain"
        );
        assert_eq!(seen.authorization.as_deref(), Some("Bearer good"));
        assert_eq!(seen.accept_encoding.as_deref(), Some("identity"));
    }

    #[tokio::test]
    async fn an_old_homeserver_gets_the_legacy_endpoint_without_a_token() {
        let (address, seen) = mock_homeserver().await;
        let client = sdk_client(address, MatrixVersion::V1_1, "good").await;
        let (bytes, _) = drain(open_plain(&client, "plain").await.unwrap().body).await;
        assert_eq!(bytes, b"hello media");
        let seen = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!(seen.path, "/_matrix/media/v3/download/example.org/plain");
        assert_eq!(seen.authorization, None);
    }

    #[tokio::test]
    async fn a_body_outlasting_the_sdk_request_timeout_still_arrives() {
        let (address, _) = mock_homeserver().await;
        let client = sdk_client(address, MatrixVersion::V1_11, "good").await;
        let media = open_plain(&client, "slow").await.unwrap();
        let (bytes, error) = drain(media.body).await;
        assert!(
            error.is_none(),
            "the client's 300 ms timeout cut the body: {error:?}"
        );
        assert_eq!(bytes.len(), 5 * 1024);
    }

    #[tokio::test]
    async fn an_unknown_token_is_refreshed_once_and_retried() {
        let (address, seen) = mock_homeserver().await;
        let client = sdk_client(address, MatrixVersion::V1_11, "stale").await;
        let (bytes, _) = drain(open_plain(&client, "plain").await.unwrap().body).await;
        assert_eq!(bytes, b"hello media");
        // Only the media requests: the SDK makes its own in the background.
        let auths: Vec<_> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.path.contains("/media/download/"))
            .map(|seen| seen.authorization.clone())
            .collect();
        assert_eq!(
            auths,
            [
                Some("Bearer stale".to_owned()),
                Some("Bearer fresh".to_owned())
            ]
        );
    }

    #[tokio::test]
    async fn error_answers_and_encoded_bodies_are_classified() {
        let (address, _) = mock_homeserver().await;
        let client = sdk_client(address, MatrixVersion::V1_11, "good").await;
        assert!(matches!(
            open_plain(&client, "missing").await,
            Err(GatewayError::MediaNotFound(_))
        ));
        assert!(matches!(
            open_plain(&client, "broken").await,
            Err(GatewayError::Upstream(message)) if message.contains("502")
        ));
        assert!(matches!(
            open_plain(&client, "brotli").await,
            Err(GatewayError::Upstream(message)) if message.contains("Content-Encoding")
        ));
    }

    /// Media requests the mock saw for `media_id`.
    fn requests_for(seen: &Mutex<Vec<Seen>>, media_id: &str) -> usize {
        let suffix = format!("/media/download/example.org/{media_id}");
        seen.lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.path.ends_with(&suffix))
            .count()
    }

    /// The SDK retries `429` and `5xx` for its own buffered download, so the
    /// streamed one does too, honouring `retry_after_ms`, a bounded number
    /// of times.
    #[tokio::test]
    async fn transient_answers_are_retried_a_bounded_number_of_times() {
        let (address, seen) = mock_homeserver().await;
        let client = sdk_client(address, MatrixVersion::V1_11, "good").await;

        for media_id in ["limited", "flaky"] {
            let media = open_plain(&client, media_id).await.unwrap();
            let (bytes, error) = drain(media.body).await;
            assert!(error.is_none(), "{media_id}: {error:?}");
            assert_eq!(bytes, b"hello media", "{media_id}");
            assert_eq!(requests_for(&seen, media_id), 2, "{media_id}");
        }

        assert!(matches!(
            open_plain(&client, "broken").await,
            Err(GatewayError::Upstream(message)) if message.contains("502")
        ));
        assert_eq!(requests_for(&seen, "broken"), MAX_ATTEMPTS as usize);

        // A permanent answer is not retried.
        assert!(open_plain(&client, "missing").await.is_err());
        assert_eq!(requests_for(&seen, "missing"), 1);
    }

    #[tokio::test]
    async fn an_unknown_token_without_a_refresh_token_fails_once_and_says_so() {
        let (address, seen) = mock_homeserver().await;
        let client = sdk_client_with(address, MatrixVersion::V1_11, "stale", None).await;
        assert!(matches!(
            open_plain(&client, "plain").await,
            Err(GatewayError::Upstream(message)) if message.contains("M_UNKNOWN_TOKEN")
        ));
        assert_eq!(requests_for(&seen, "plain"), 1);
    }

    #[tokio::test]
    async fn a_gzip_answer_is_decoded_and_streamed_without_a_length() {
        let (address, _) = mock_homeserver().await;
        let client = sdk_client(address, MatrixVersion::V1_11, "good").await;
        let media = open_plain(&client, "gzipped").await.unwrap();
        // The compressed length is not the media's, so none is claimed.
        assert_eq!(media.len, None);
        let (bytes, error) = drain(media.body).await;
        assert!(error.is_none());
        assert_eq!(bytes, b"hello media");
    }

    #[test]
    fn download_urls_follow_the_endpoint_and_keep_a_path_prefix() {
        let base = Url::parse("https://hs.example.org/").unwrap();
        assert_eq!(
            download_url(&base, "example.org", "abc", true)
                .unwrap()
                .as_str(),
            "https://hs.example.org/_matrix/client/v1/media/download/example.org/abc"
        );
        assert_eq!(
            download_url(&base, "example.org", "abc", false)
                .unwrap()
                .as_str(),
            "https://hs.example.org/_matrix/media/v3/download/example.org/abc"
        );
        let prefixed = Url::parse("https://example.org/matrix").unwrap();
        assert_eq!(
            download_url(&prefixed, "example.org:8448", "a_b-C", true)
                .unwrap()
                .as_str(),
            "https://example.org/matrix/_matrix/client/v1/media/download/example.org:8448/a_b-C"
        );
    }
}
