//! Media proxy endpoint: `GET /v1/media/{account_id}/{server_name}/{media_id}`.
//!
//! The handler reconstructs the `mxc://` URI from the path components, looks up
//! the owning event (to discover the encrypted-file descriptor and the MIME
//! type), delegates the authenticated download-and-cache to the injected
//! [`MediaProxy`], then streams the resulting file back — honoring HTTP range
//! requests and conditional (`If-None-Match`) GETs, with `ETag` /
//! `Accept-Ranges` / `Content-Type` set. The body is raw binary media, not the
//! `{data}` JSON envelope.

use std::io::SeekFrom;
use std::sync::Arc;

use axon_store::Store;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::dto::ThumbnailQuery;
use crate::media::{MediaError, MediaProxy, MediaResource};
use crate::response::ApiError;

/// Dimension clamp bounds for the thumbnail route — a requested `width`/
/// `height` outside this range is clamped, not rejected (same "clamp, don't
/// 400" convention as the pagination `limit` in `rooms.rs`/`search.rs`).
const MIN_THUMBNAIL_DIMENSION: u32 = 16;
const MAX_THUMBNAIL_DIMENSION: u32 = 1600;

/// Standard sizes a clamped dimension snaps *up* to (never down, so the
/// served thumbnail is never smaller than requested). Two purposes: it bounds
/// how many distinct `(width, height, method)` cache entries a single
/// `mxc_url` can accumulate (an unbounded range would otherwise let a client
/// multiply cache entries per pixel requested), and it mirrors the small
/// preset list a homeserver running with on-demand thumbnailing disabled
/// (e.g. Synapse's default `dynamic_thumbnails: false`) actually serves, so a
/// request is more likely to match a size the homeserver has pre-generated
/// instead of failing against every non-preset size. `MAX_THUMBNAIL_DIMENSION`
/// is included so the clamp's own ceiling is always representable.
const THUMBNAIL_SIZE_BUCKETS: &[u32] = &[32, 96, 320, 640, 800, MAX_THUMBNAIL_DIMENSION];

/// Clamp `requested` into `[MIN_THUMBNAIL_DIMENSION, MAX_THUMBNAIL_DIMENSION]`,
/// then snap up to the smallest [`THUMBNAIL_SIZE_BUCKETS`] entry that still
/// covers it (falling back to the clamp ceiling if none does, which cannot
/// currently happen since the ceiling is itself the last bucket).
fn snap_thumbnail_dimension(requested: u32) -> u32 {
    let clamped = requested.clamp(MIN_THUMBNAIL_DIMENSION, MAX_THUMBNAIL_DIMENSION);
    THUMBNAIL_SIZE_BUCKETS
        .iter()
        .copied()
        .find(|&bucket| bucket >= clamped)
        .unwrap_or(MAX_THUMBNAIL_DIMENSION)
}

/// Return the encrypted-file descriptor whose URL matches the requested MXC.
///
/// Primary encrypted attachments use `content.file`; encrypted thumbnails use
/// `content.info.thumbnail_file`. Plain media and non-matching descriptors
/// return `None`.
fn encrypted_file_for_mxc(content: &Value, mxc_url: &str) -> Option<Value> {
    let candidates = [
        content.get("file"),
        content
            .get("info")
            .and_then(|info| info.get("thumbnail_file")),
    ];

    candidates.into_iter().flatten().find_map(|file| {
        (file.get("url").and_then(Value::as_str) == Some(mxc_url)).then(|| file.clone())
    })
}

/// Derive the MIME type to serve from the event content — the authoritative
/// source, since the SDK download does not surface a `Content-Type`.
///
/// A primary attachment (matched via `content.url` or `content.file.url`) uses
/// `content.info.mimetype`; a thumbnail (matched via `content.info.
/// thumbnail_url` or `content.info.thumbnail_file.url`) uses
/// `content.info.thumbnail_info.mimetype`. The value is sender-controlled, so
/// it is sanitized (against header injection) *and* restricted to an
/// inline-safe allowlist (see [`is_inline_safe`]): anything unusable or not
/// inline-safe falls back to `application/octet-stream`. Combined with the
/// `X-Content-Type-Options: nosniff` header the handler always sets, this
/// prevents a hostile attachment declaring `text/html` / `image/svg+xml` from
/// being rendered as active content by a browser-based client.
fn content_type_for_mxc(content: &Value, mxc_url: &str) -> String {
    let info = content.get("info");

    let primary_url = content.get("url").and_then(Value::as_str).or_else(|| {
        content
            .get("file")
            .and_then(|f| f.get("url"))
            .and_then(Value::as_str)
    });
    let thumbnail_url = info
        .and_then(|i| i.get("thumbnail_url"))
        .and_then(Value::as_str)
        .or_else(|| {
            info.and_then(|i| i.get("thumbnail_file"))
                .and_then(|f| f.get("url"))
                .and_then(Value::as_str)
        });

    let raw = if primary_url == Some(mxc_url) {
        info.and_then(|i| i.get("mimetype")).and_then(Value::as_str)
    } else if thumbnail_url == Some(mxc_url) {
        info.and_then(|i| i.get("thumbnail_info"))
            .and_then(|ti| ti.get("mimetype"))
            .and_then(Value::as_str)
    } else {
        None
    };

    raw.and_then(sanitize_mime)
        .filter(|mime| is_inline_safe(mime))
        .unwrap_or_else(|| "application/octet-stream".to_owned())
}

/// Whether a sanitized MIME type is safe to serve with its declared type for
/// inline rendering. Only images (excluding SVG, which can carry script), audio,
/// and video are allowed through; everything else (`text/html`, `image/svg+xml`,
/// `application/javascript`, `application/pdf`, …) is downgraded to
/// `application/octet-stream` so a client cannot be tricked into executing
/// attacker-supplied active content.
fn is_inline_safe(mime: &str) -> bool {
    if mime == "image/svg+xml" {
        return false;
    }
    mime.starts_with("image/") || mime.starts_with("audio/") || mime.starts_with("video/")
}

/// Accept only a well-formed `type/subtype` MIME token (no parameters), guarding
/// against header injection from hostile event content. Returns `None` for
/// anything unusable so the caller falls back to `application/octet-stream`.
fn sanitize_mime(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 255 {
        return None;
    }
    let (ty, subtype) = raw.split_once('/')?;
    let is_token = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|b| {
                b.is_ascii_alphanumeric()
                    || matches!(
                        b,
                        b'!' | b'#' | b'$' | b'&' | b'-' | b'^' | b'_' | b'.' | b'+'
                    )
            })
    };
    (is_token(ty) && is_token(subtype)).then(|| format!("{ty}/{subtype}"))
}

/// The outcome of interpreting a `Range` header against a known content length.
#[derive(Debug, PartialEq, Eq)]
enum RangeSpec {
    /// No (or an ignorable/malformed) range — serve the full body with `200`.
    Full,
    /// A single satisfiable byte range (inclusive) — serve `206`.
    Satisfiable { start: u64, end: u64 },
    /// A syntactically valid range that falls outside the content — serve `416`.
    Unsatisfiable,
}

/// Parse a single-range `Range: bytes=…` header against `len`.
///
/// Per RFC 7233 a malformed or unsupported (e.g. multi-range) header is ignored
/// (→ full body); a well-formed range wholly outside the content is
/// unsatisfiable (→ `416`).
fn parse_range(header: Option<&str>, len: u64) -> RangeSpec {
    let Some(raw) = header else {
        return RangeSpec::Full;
    };
    let Some(spec) = raw.strip_prefix("bytes=") else {
        return RangeSpec::Full; // unknown unit → ignore
    };
    let spec = spec.trim();
    if spec.contains(',') {
        return RangeSpec::Full; // multi-range unsupported → serve full
    }
    let Some((start_s, end_s)) = spec.split_once('-') else {
        return RangeSpec::Full;
    };

    if start_s.is_empty() {
        // Suffix range: bytes=-N → final N bytes.
        return match end_s.parse::<u64>() {
            Ok(0) => RangeSpec::Unsatisfiable,
            Ok(n) if len == 0 => {
                let _ = n;
                RangeSpec::Unsatisfiable
            }
            Ok(n) => {
                let n = n.min(len);
                RangeSpec::Satisfiable {
                    start: len - n,
                    end: len - 1,
                }
            }
            Err(_) => RangeSpec::Full,
        };
    }

    let Ok(start) = start_s.parse::<u64>() else {
        return RangeSpec::Full;
    };
    if len == 0 || start >= len {
        return RangeSpec::Unsatisfiable;
    }
    let end = if end_s.is_empty() {
        len - 1
    } else {
        match end_s.parse::<u64>() {
            Ok(e) => e.min(len - 1),
            Err(_) => return RangeSpec::Full,
        }
    };
    if end < start {
        return RangeSpec::Full; // invalid range → ignore
    }
    RangeSpec::Satisfiable { start, end }
}

/// Whether an `If-None-Match` header matches our (quoted) entity tag, so the
/// handler can answer `304 Not Modified`.
fn if_none_match_matches(header: Option<&str>, etag_quoted: &str) -> bool {
    let Some(header) = header else {
        return false;
    };
    header
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == "*" || candidate.trim_start_matches("W/") == etag_quoted)
}

/// Proxy an `mxc://` download through the account's homeserver connection.
///
/// The `server_name` and `media_id` path segments form the `mxc://` URI that
/// was embedded in a Matrix event's primary or thumbnail media descriptor.
/// The response body is the raw media bytes, streamed from the on-disk cache
/// with range-request and conditional-GET support.
#[utoipa::path(
    get,
    path = "/v1/media/{account_id}/{server_name}/{media_id}",
    params(
        ("account_id" = Uuid, Path, description = "Axon account whose credentials are used for the download"),
        ("server_name" = String, Path, description = "Server-name component of the MXC URI (the part after `mxc://`)"),
        ("media_id" = String, Path, description = "Media-ID component of the MXC URI"),
    ),
    responses(
        (status = 200, description = "Full media bytes. `Content-Type` is derived from the event (inline-safe types only; else `application/octet-stream`), with `X-Content-Type-Options: nosniff`, `Accept-Ranges: bytes`, and `ETag` set."),
        (status = 206, description = "Partial media bytes for a satisfiable `Range` request (with `Content-Range`)."),
        (status = 304, description = "The `If-None-Match` entity tag matched; body omitted."),
        (status = 400, description = "Syntactically invalid MXC URI components", body = crate::response::ErrorResponse),
        (status = 403, description = "The homeserver denied access to this media object", body = crate::response::ErrorResponse),
        (status = 404, description = "Account not found, or media not found on the homeserver", body = crate::response::ErrorResponse),
        (status = 413, description = "The media object exceeds the configured per-object cache limit", body = crate::response::ErrorResponse),
        (status = 422, description = "The encrypted attachment arrived intact but could not be decrypted (`media_undecryptable`). Terminal — retrying will not help.", body = crate::response::ErrorResponse),
        (status = 416, description = "The requested `Range` is not satisfiable (with `Content-Range: bytes */len`).", body = crate::response::ErrorResponse),
        (status = 500, description = "Internal media-metadata lookup failure", body = crate::response::ErrorResponse),
        (status = 502, description = "The homeserver was unreachable or returned an error", body = crate::response::ErrorResponse),
        (status = 503, description = "The account's homeserver connection is not currently established", body = crate::response::ErrorResponse),
    ),
    tag = "media",
)]
pub async fn get_media(
    State(proxy): State<Arc<dyn MediaProxy>>,
    State(store): State<Store>,
    Path((account_id, server_name, media_id)): Path<(Uuid, String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let mxc_url = format!("mxc://{server_name}/{media_id}");

    let content = resolve_media_content(&store, account_id, &mxc_url).await?;

    // Conditional GET: the entity tag is content-addressed (a hash of the MXC
    // URI) and needs no download, so answer `304` *before* fetching. This is
    // safe here because the fail-closed event checks above have already passed
    // (the event exists and is decrypted), so a matching tag means the client
    // already holds the current bytes.
    let etag = format!("\"{}\"", proxy.etag(&mxc_url));
    let if_none_match = header_str(&headers, header::IF_NONE_MATCH);
    if if_none_match_matches(if_none_match.as_deref(), &etag) {
        return Ok(not_modified(&etag));
    }

    let encrypted_file = encrypted_file_for_mxc(&content, &mxc_url);
    let content_type = content_type_for_mxc(&content, &mxc_url);

    let resource = proxy
        .get_media(account_id, &mxc_url, encrypted_file)
        .await
        .map_err(|err| log_and_convert_media_error(account_id, &mxc_url, "get_media", err))?;

    let range = header_str(&headers, header::RANGE);
    serve_media(account_id, resource, content_type, range, etag).await
}

/// Validate a thumbnail request against the resolved event content, producing
/// the [`ThumbnailSpec`](axon_core::media::ThumbnailSpec) to fetch — or a
/// `400` if the media is encrypted (a homeserver can only thumbnail
/// plaintext it can see; see the module-level note on
/// `axon-sync::SdkMediaProxy::download_thumbnail`). Pure — no I/O — so it's
/// unit-testable without a store or homeserver.
fn resolve_thumbnail_spec(
    content: &Value,
    mxc_url: &str,
    query: ThumbnailQuery,
) -> Result<axon_core::media::ThumbnailSpec, ApiError> {
    if encrypted_file_for_mxc(content, mxc_url).is_some() {
        return Err(ApiError::bad_request(format!(
            "cannot generate a homeserver thumbnail of encrypted media: {mxc_url}"
        )));
    }
    let width = snap_thumbnail_dimension(query.width);
    let height = snap_thumbnail_dimension(query.height);
    let method = query
        .method
        .map(axon_core::media::ThumbnailMethod::from)
        .unwrap_or(axon_core::media::ThumbnailMethod::Scale);
    Ok(axon_core::media::ThumbnailSpec {
        width,
        height,
        method,
    })
}

/// Recognize a well-known image format from its leading magic bytes. Used to
/// correct `Content-Type` for a homeserver-generated thumbnail, which may be
/// re-encoded into a different format than the original's declared mimetype
/// (e.g. a PNG source thumbnailed to JPEG by Synapse) — sniffing the actual
/// returned bytes is strictly more trustworthy here than trusting the
/// original's sender-controlled declaration, since the bytes themselves are
/// homeserver-generated. Only recognizes formats already in
/// [`is_inline_safe`]'s allowlist, so a positive match is always safe to
/// serve with its own declared type.
fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xFF\xD8\xFF") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Correct `declared` (the original's `Content-Type`, from
/// [`content_type_for_mxc`]) by sniffing the actual thumbnail bytes' magic
/// number — see [`sniff_image_mime`]. Falls back to `declared` unchanged when
/// the bytes don't match a recognized signature (e.g. the homeserver returned
/// a format this function doesn't recognize).
///
/// Reads a few leading bytes and seeks the file back to the start, so it must
/// run before any other read/seek on `resource.file` — in particular, before
/// [`serve_media`], which assumes the cursor starts at `0` for a full-body
/// response.
async fn correct_thumbnail_content_type(resource: &mut MediaResource, declared: String) -> String {
    let mut magic = [0u8; 12];
    let n = resource.file.read(&mut magic).await.unwrap_or(0);
    let _ = resource.file.seek(SeekFrom::Start(0)).await;
    sniff_image_mime(&magic[..n])
        .map(str::to_owned)
        .unwrap_or(declared)
}

/// Resolve the event owning `mxc_url` and its content, failing closed (`404`)
/// if the event is missing, or if its content is `NULL` (redacted or a
/// not-yet-decrypted UTD event) — we cannot determine whether the media is
/// encrypted without it, and serving ciphertext as plain bytes under `200`
/// would be worse than a `404`. Shared by [`get_media`] and
/// [`get_media_thumbnail`].
async fn resolve_media_content(
    store: &Store,
    account_id: Uuid,
    mxc_url: &str,
) -> Result<Value, ApiError> {
    let event = store
        .get_event_by_mxc_url(account_id, mxc_url)
        .await?
        .ok_or_else(|| ApiError::from(MediaError::NotFound(mxc_url.to_owned())))?;

    event.content.ok_or_else(|| {
        ApiError::from(MediaError::NotFound(format!(
            "media unavailable (redacted or not yet decrypted): {mxc_url}"
        )))
    })
}

/// Convert a [`MediaProxy`] failure into the response the client sees,
/// logging a `MediaError::Internal` detail (with account/mxc context) first
/// — the `500` body itself is generic, so the real cause must be logged
/// before converting, per the structured-logging convention. `route`
/// disambiguates the full-media and thumbnail call sites in the log. Shared
/// by [`get_media`] and [`get_media_thumbnail`].
fn log_and_convert_media_error(
    account_id: Uuid,
    mxc_url: &str,
    route: &'static str,
    err: MediaError,
) -> ApiError {
    match &err {
        MediaError::Internal(detail) => {
            tracing::error!(%account_id, mxc = %mxc_url, route, error = %detail, "media proxy internal error");
        }
        // The operator's only signal that an attachment is genuinely
        // undecryptable. Before issue #359 this detail went to the client and
        // nowhere else, so a report of "it won't decrypt" left nothing in the
        // log but a bare `502` indistinguishable from a homeserver blip.
        MediaError::Undecryptable(detail) => {
            tracing::warn!(%account_id, mxc = %mxc_url, route, error = %detail, "media could not be decrypted");
        }
        _ => {}
    }
    err.into()
}

/// Proxy a homeserver-generated thumbnail of an `mxc://` object through the
/// account's homeserver connection.
///
/// Shares [`get_media`]'s event lookup (via [`resolve_media_content`]) and
/// conditional-GET/range-serving machinery, but resolves a resized variant
/// via [`MediaProxy::get_thumbnail`] instead of the full original, and
/// corrects `Content-Type` by sniffing the returned bytes (see
/// [`correct_thumbnail_content_type`]) since a homeserver-regenerated
/// thumbnail may not match the original's declared mimetype. Encrypted media
/// is rejected with `400` before the proxy is ever called — see
/// [`resolve_thumbnail_spec`].
#[utoipa::path(
    get,
    path = "/v1/media/{account_id}/{server_name}/{media_id}/thumbnail",
    params(
        ("account_id" = Uuid, Path, description = "Axon account whose credentials are used for the download"),
        ("server_name" = String, Path, description = "Server-name component of the MXC URI (the part after `mxc://`)"),
        ("media_id" = String, Path, description = "Media-ID component of the MXC URI"),
        ThumbnailQuery,
    ),
    responses(
        (status = 200, description = "Thumbnail bytes, generated by the homeserver. `Content-Type` is the original's declared MIME type, corrected by sniffing the returned bytes' magic number if the homeserver re-encoded to a different recognized image format (e.g. a PNG source thumbnailed to JPEG); `X-Content-Type-Options: nosniff`, `Accept-Ranges: bytes`, and `ETag` are set."),
        (status = 206, description = "Partial thumbnail bytes for a satisfiable `Range` request (with `Content-Range`)."),
        (status = 304, description = "The `If-None-Match` entity tag matched; body omitted."),
        (status = 400, description = "Syntactically invalid MXC URI components, or the media is encrypted — a homeserver cannot thumbnail ciphertext it cannot decrypt", body = crate::response::ErrorResponse),
        (status = 403, description = "The homeserver denied access to this media object", body = crate::response::ErrorResponse),
        (status = 404, description = "Account not found, or media not found on the homeserver", body = crate::response::ErrorResponse),
        (status = 413, description = "The thumbnail exceeds the configured per-object cache limit", body = crate::response::ErrorResponse),
        (status = 416, description = "The requested `Range` is not satisfiable (with `Content-Range: bytes */len`).", body = crate::response::ErrorResponse),
        (status = 500, description = "Internal media-metadata lookup failure", body = crate::response::ErrorResponse),
        (status = 502, description = "The homeserver was unreachable or returned an error", body = crate::response::ErrorResponse),
        (status = 503, description = "The account's homeserver connection is not currently established", body = crate::response::ErrorResponse),
    ),
    tag = "media",
)]
pub async fn get_media_thumbnail(
    State(proxy): State<Arc<dyn MediaProxy>>,
    State(store): State<Store>,
    Path((account_id, server_name, media_id)): Path<(Uuid, String, String)>,
    crate::extract::Query(query): crate::extract::Query<ThumbnailQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let mxc_url = format!("mxc://{server_name}/{media_id}");

    let content = resolve_media_content(&store, account_id, &mxc_url).await?;

    let spec = resolve_thumbnail_spec(&content, &mxc_url, query)?;

    let etag = format!("\"{}\"", proxy.etag_thumbnail(&mxc_url, spec));
    let if_none_match = header_str(&headers, header::IF_NONE_MATCH);
    if if_none_match_matches(if_none_match.as_deref(), &etag) {
        return Ok(not_modified(&etag));
    }

    let declared_content_type = content_type_for_mxc(&content, &mxc_url);

    let mut resource = proxy
        .get_thumbnail(account_id, &mxc_url, spec)
        .await
        .map_err(|err| log_and_convert_media_error(account_id, &mxc_url, "get_thumbnail", err))?;

    // The homeserver may have re-encoded the thumbnail into a different
    // format than the original's declared mimetype — see
    // `correct_thumbnail_content_type`'s doc.
    let content_type = correct_thumbnail_content_type(&mut resource, declared_content_type).await;

    let range = header_str(&headers, header::RANGE);
    serve_media(account_id, resource, content_type, range, etag).await
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Build a `304 Not Modified` response carrying the entity tag.
fn not_modified(etag: &str) -> Response {
    Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .header(header::ETAG, etag)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(Body::empty())
        .expect("valid 304 response")
}

/// Build the media response for an open [`MediaResource`], applying range
/// serving and the standard media headers (`etag` is the already-quoted entity
/// tag). Conditional-GET (`304`) is handled by the caller before fetching.
/// Factored out so it can be unit-tested without a store or homeserver.
async fn serve_media(
    account_id: Uuid,
    mut resource: MediaResource,
    content_type: String,
    range: Option<String>,
    etag: String,
) -> Result<Response, ApiError> {
    match parse_range(range.as_deref(), resource.len) {
        RangeSpec::Unsatisfiable => {
            // Reuse the shared JSON error envelope (advertised by the OpenAPI
            // spec for this status) rather than an empty body, then layer the
            // range-specific headers on top.
            let mut response = ApiError::range_not_satisfiable(format!(
                "requested range is not satisfiable for a {}-byte resource",
                resource.len
            ))
            .into_response();
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{}", resource.len))
                    .expect("valid header value"),
            );
            headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            headers.insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            headers.insert(
                header::ETAG,
                HeaderValue::from_str(&etag).expect("valid header value"),
            );
            Ok(response)
        }

        RangeSpec::Satisfiable { start, end } => {
            resource
                .file
                .seek(SeekFrom::Start(start))
                .await
                .map_err(|e| {
                    tracing::error!(%account_id, error = %e, "media cache seek failed");
                    ApiError::internal()
                })?;
            let length = end - start + 1;
            let body = Body::from_stream(ReaderStream::new(resource.file.take(length)));
            Ok(Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_LENGTH, length)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{}", resource.len),
                )
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
                .header(header::ETAG, &etag)
                .body(body)
                .expect("valid 206 response"))
        }

        RangeSpec::Full => {
            let body = Body::from_stream(ReaderStream::new(resource.file));
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_LENGTH, resource.len)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
                .header(header::ETAG, &etag)
                .body(body)
                .expect("valid 200 response"))
        }
    }
    .map(IntoResponse::into_response)
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use serde_json::json;

    use super::*;
    use crate::media::{MediaError, MediaResource};

    #[test]
    fn selects_primary_encrypted_file_by_url() {
        let content = json!({
            "file": { "url": "mxc://example.org/main", "key": "main" },
            "info": {
                "thumbnail_file": {
                    "url": "mxc://example.org/thumb",
                    "key": "thumb"
                }
            }
        });

        assert_eq!(
            encrypted_file_for_mxc(&content, "mxc://example.org/main")
                .and_then(|file| file.get("key").cloned()),
            Some(json!("main"))
        );
    }

    #[test]
    fn selects_encrypted_thumbnail_by_url() {
        let content = json!({
            "file": { "url": "mxc://example.org/main", "key": "main" },
            "info": {
                "thumbnail_file": {
                    "url": "mxc://example.org/thumb",
                    "key": "thumb"
                }
            }
        });

        assert_eq!(
            encrypted_file_for_mxc(&content, "mxc://example.org/thumb")
                .and_then(|file| file.get("key").cloned()),
            Some(json!("thumb"))
        );
    }

    #[test]
    fn resolve_thumbnail_spec_rejects_encrypted_media() {
        let content = json!({
            "file": { "url": "mxc://example.org/main", "key": "main" }
        });
        let query = ThumbnailQuery {
            width: 64,
            height: 64,
            method: None,
        };
        let err = resolve_thumbnail_spec(&content, "mxc://example.org/main", query).unwrap_err();
        assert_eq!(err.into_response().status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn resolve_thumbnail_spec_clamps_and_snaps_dimensions() {
        let content = json!({ "url": "mxc://example.org/main" });
        let query = ThumbnailQuery {
            width: 1,
            height: 1_000_000,
            method: None,
        };
        let spec = resolve_thumbnail_spec(&content, "mxc://example.org/main", query).unwrap();
        // width=1 clamps up to MIN_THUMBNAIL_DIMENSION (16), then snaps up to
        // the smallest covering bucket (32).
        assert_eq!(spec.width, 32);
        // height=1_000_000 clamps down to MAX_THUMBNAIL_DIMENSION (1600),
        // which is itself the last bucket.
        assert_eq!(spec.height, MAX_THUMBNAIL_DIMENSION);
    }

    #[test]
    fn snap_thumbnail_dimension_snaps_up_to_nearest_bucket() {
        assert_eq!(snap_thumbnail_dimension(1), 32);
        assert_eq!(snap_thumbnail_dimension(32), 32);
        assert_eq!(snap_thumbnail_dimension(33), 96);
        assert_eq!(snap_thumbnail_dimension(96), 96);
        assert_eq!(snap_thumbnail_dimension(321), 640);
    }

    #[test]
    fn snap_thumbnail_dimension_never_undersizes_or_exceeds_the_ceiling() {
        for requested in [1, 16, 32, 100, 321, 801, 1600, 1_000_000] {
            let snapped = snap_thumbnail_dimension(requested);
            assert!(
                snapped >= requested.clamp(MIN_THUMBNAIL_DIMENSION, MAX_THUMBNAIL_DIMENSION),
                "snap({requested}) = {snapped} must never undersize the clamped request"
            );
            assert!(snapped <= MAX_THUMBNAIL_DIMENSION);
        }
    }

    #[test]
    fn sniff_image_mime_recognizes_common_formats() {
        assert_eq!(
            sniff_image_mime(b"\x89PNG\r\n\x1a\nrest"),
            Some("image/png")
        );
        assert_eq!(sniff_image_mime(b"\xFF\xD8\xFFrest"), Some("image/jpeg"));
        assert_eq!(sniff_image_mime(b"GIF89arest"), Some("image/gif"));
        assert_eq!(
            sniff_image_mime(b"RIFF\x00\x00\x00\x00WEBPrest"),
            Some("image/webp")
        );
        assert_eq!(sniff_image_mime(b"not an image"), None);
        assert_eq!(sniff_image_mime(b""), None);
    }

    #[test]
    fn resolve_thumbnail_spec_defaults_method_to_scale() {
        let content = json!({ "url": "mxc://example.org/main" });
        let query = ThumbnailQuery {
            width: 64,
            height: 64,
            method: None,
        };
        let spec = resolve_thumbnail_spec(&content, "mxc://example.org/main", query).unwrap();
        assert_eq!(spec.method, axon_core::media::ThumbnailMethod::Scale);
    }

    #[test]
    fn resolve_thumbnail_spec_honors_explicit_crop() {
        let content = json!({ "url": "mxc://example.org/main" });
        let query = ThumbnailQuery {
            width: 64,
            height: 64,
            method: Some(crate::dto::ThumbnailMethodDto::Crop),
        };
        let spec = resolve_thumbnail_spec(&content, "mxc://example.org/main", query).unwrap();
        assert_eq!(spec.method, axon_core::media::ThumbnailMethod::Crop);
    }

    #[test]
    fn ignores_nonmatching_encrypted_descriptors() {
        let content = json!({
            "file": { "url": "mxc://example.org/other" },
            "info": { "thumbnail_url": "mxc://example.org/plain-thumb" }
        });

        assert!(encrypted_file_for_mxc(&content, "mxc://example.org/plain-thumb").is_none());
    }

    #[test]
    fn content_type_prefers_primary_mimetype() {
        let content = json!({
            "url": "mxc://example.org/main",
            "info": { "mimetype": "image/png" }
        });
        assert_eq!(
            content_type_for_mxc(&content, "mxc://example.org/main"),
            "image/png"
        );
    }

    #[test]
    fn content_type_uses_thumbnail_mimetype_for_thumb() {
        let content = json!({
            "url": "mxc://example.org/main",
            "info": {
                "mimetype": "image/png",
                "thumbnail_url": "mxc://example.org/thumb",
                "thumbnail_info": { "mimetype": "image/jpeg" }
            }
        });
        assert_eq!(
            content_type_for_mxc(&content, "mxc://example.org/thumb"),
            "image/jpeg"
        );
    }

    #[test]
    fn content_type_falls_back_to_octet_stream() {
        let content = json!({ "url": "mxc://example.org/main", "info": {} });
        assert_eq!(
            content_type_for_mxc(&content, "mxc://example.org/main"),
            "application/octet-stream"
        );
    }

    #[test]
    fn content_type_downgrades_active_and_scriptable_types() {
        // A hostile attachment declaring an active/scriptable type is coerced to
        // octet-stream so a browser client can't render it as executable content.
        for dangerous in ["text/html", "image/svg+xml", "application/javascript"] {
            let content =
                json!({ "url": "mxc://example.org/x", "info": { "mimetype": dangerous } });
            assert_eq!(
                content_type_for_mxc(&content, "mxc://example.org/x"),
                "application/octet-stream",
                "{dangerous} must be downgraded"
            );
        }
        // Inline-safe media keeps its type.
        for safe in ["image/png", "audio/ogg", "video/mp4"] {
            let content = json!({ "url": "mxc://example.org/x", "info": { "mimetype": safe } });
            assert_eq!(content_type_for_mxc(&content, "mxc://example.org/x"), safe);
        }
    }

    #[test]
    fn hostile_mimetype_is_rejected() {
        // Header-injection attempt and control characters are refused.
        assert_eq!(sanitize_mime("image/png\r\nSet-Cookie: x"), None);
        assert_eq!(sanitize_mime("not-a-mime"), None);
        assert_eq!(sanitize_mime("image/png; charset=x"), None);
        assert_eq!(sanitize_mime("image/png"), Some("image/png".to_owned()));
    }

    #[test]
    fn range_parsing_covers_the_cases() {
        assert_eq!(parse_range(None, 100), RangeSpec::Full);
        // A range covering the whole body is still a (satisfiable) range → 206.
        assert_eq!(
            parse_range(Some("bytes=0-99"), 100),
            RangeSpec::Satisfiable { start: 0, end: 99 }
        );
        assert_eq!(
            parse_range(Some("bytes=0-49"), 100),
            RangeSpec::Satisfiable { start: 0, end: 49 }
        );
        // Open-ended.
        assert_eq!(
            parse_range(Some("bytes=50-"), 100),
            RangeSpec::Satisfiable { start: 50, end: 99 }
        );
        // Suffix.
        assert_eq!(
            parse_range(Some("bytes=-10"), 100),
            RangeSpec::Satisfiable { start: 90, end: 99 }
        );
        // Clamped end.
        assert_eq!(
            parse_range(Some("bytes=0-999"), 100),
            RangeSpec::Satisfiable { start: 0, end: 99 }
        );
        // Out of bounds → unsatisfiable.
        assert_eq!(
            parse_range(Some("bytes=200-300"), 100),
            RangeSpec::Unsatisfiable
        );
        // Multi-range unsupported → full.
        assert_eq!(parse_range(Some("bytes=0-1,2-3"), 100), RangeSpec::Full);
        // Malformed → full.
        assert_eq!(parse_range(Some("chars=0-1"), 100), RangeSpec::Full);
    }

    #[test]
    fn if_none_match_recognizes_star_and_tag() {
        assert!(if_none_match_matches(Some("*"), "\"abc\""));
        assert!(if_none_match_matches(Some("\"abc\""), "\"abc\""));
        assert!(if_none_match_matches(Some("\"x\", \"abc\""), "\"abc\""));
        assert!(if_none_match_matches(Some("W/\"abc\""), "\"abc\""));
        assert!(!if_none_match_matches(Some("\"other\""), "\"abc\""));
        assert!(!if_none_match_matches(None, "\"abc\""));
    }

    async fn resource_from(bytes: &[u8]) -> MediaResource {
        use tokio::io::AsyncWriteExt;
        let dir = std::env::temp_dir().join(format!("axon-media-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("obj");
        let mut f = tokio::fs::File::create(&path).await.unwrap();
        f.write_all(bytes).await.unwrap();
        f.sync_all().await.unwrap();
        drop(f);
        let file = tokio::fs::File::open(&path).await.unwrap();
        let _ = std::fs::remove_file(&path); // fd survives unlink
        MediaResource {
            file,
            len: bytes.len() as u64,
            etag: "unused".to_owned(), // serve_media takes the etag as an argument
        }
    }

    #[tokio::test]
    async fn correct_thumbnail_content_type_overrides_on_recognized_signature() {
        let mut resource = resource_from(b"\x89PNG\r\n\x1a\nrest-of-file").await;
        let corrected =
            correct_thumbnail_content_type(&mut resource, "image/jpeg".to_owned()).await;
        assert_eq!(corrected, "image/png");
        // The cursor must be back at the start for the subsequent full-body serve.
        let mut buf = Vec::new();
        resource.file.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"\x89PNG\r\n\x1a\nrest-of-file");
    }

    #[tokio::test]
    async fn correct_thumbnail_content_type_falls_back_when_unrecognized() {
        let mut resource = resource_from(b"not an image").await;
        let corrected = correct_thumbnail_content_type(&mut resource, "image/png".to_owned()).await;
        assert_eq!(corrected, "image/png");
    }

    #[tokio::test]
    async fn full_response_sets_headers_and_body() {
        let resource = resource_from(b"hello world").await;
        let response = serve_media(
            Uuid::new_v4(),
            resource,
            "image/png".to_owned(),
            None,
            "\"abc\"".to_owned(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert_eq!(response.headers()[header::ETAG], "\"abc\"");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "11");
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"hello world");
    }

    #[tokio::test]
    async fn range_request_returns_206_with_content_range() {
        let resource = resource_from(b"0123456789").await;
        let response = serve_media(
            Uuid::new_v4(),
            resource,
            "application/octet-stream".to_owned(),
            Some("bytes=2-5".to_owned()),
            "\"abc\"".to_owned(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "4");
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"2345");
    }

    #[tokio::test]
    async fn not_modified_response_is_304_with_etag() {
        let response = not_modified("\"abc\"");
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[header::ETAG], "\"abc\"");
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn unsatisfiable_range_returns_416() {
        let resource = resource_from(b"short").await;
        let response = serve_media(
            Uuid::new_v4(),
            resource,
            "image/png".to_owned(),
            Some("bytes=100-200".to_owned()),
            "\"abc\"".to_owned(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */5");
        assert_eq!(response.headers()[header::ETAG], "\"abc\"");

        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("JSON body");
        assert_eq!(json["error"]["code"], "range_not_satisfiable");
    }

    #[tokio::test]
    async fn media_errors_use_the_shared_json_envelope() {
        let response =
            crate::response::ApiError::from(MediaError::NotFound("media missing".to_owned()))
                .into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let body = to_bytes(response.into_body(), 1024).await.expect("body");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).expect("JSON"),
            json!({
                "error": {
                    "code": "not_found",
                    "message": "media missing"
                }
            })
        );
    }
}
