//! The web client, served from this process (ADR 0116).
//!
//! A build with the `embed-web` feature carries `clients/web/dist` inside the
//! binary, and the router's fallback answers non-API paths from it. That puts
//! the SPA and `/v1/` on one origin with no reverse proxy, which is what the
//! client's default `/` API base and the absence of CORS headers both assume.
//!
//! The cache rules are the ones `deploy/web/Caddyfile` established, and for the
//! same reasons (ADR 0087):
//!
//! - `/assets/*` holds content-hashed files. A hit is immutable. A miss is a
//!   chunk a newer build no longer has, and it must be a `404` that is never
//!   cached: answering it with `index.html` makes the browser parse HTML as a
//!   module and hang a client that is still running the previous build.
//! - Every other path is a mutable name (`index.html`, `version.json`, the
//!   manifest, icons) or a client-side route, so it is revalidated on each use
//!   and an unknown path falls back to `index.html`.

use std::collections::HashMap;
use std::sync::LazyLock;

use axum::body::Body;
use axum::http::header::{
    ALLOW, CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, Method, Response, StatusCode};
use bytes::Bytes;
use sha2::{Digest, Sha256};

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

const IMMUTABLE: HeaderValue = HeaderValue::from_static("public, max-age=31536000, immutable");
const REVALIDATE: HeaderValue = HeaderValue::from_static("no-cache");
const NO_STORE: HeaderValue = HeaderValue::from_static("no-store");
const NOSNIFF: HeaderValue = HeaderValue::from_static("nosniff");

/// The bundle compiled into this binary, or `None` in a build without the
/// `embed-web` feature.
pub(crate) fn embedded() -> Option<&'static Bundle> {
    static BUNDLE: LazyLock<Option<Bundle>> = LazyLock::new(|| Bundle::new(EMBEDDED));
    BUNDLE.as_ref()
}

/// Whether this binary carries the web client. The binary uses it to say so at
/// startup and to default the bootstrap handoff to `/`.
pub fn web_client_embedded() -> bool {
    embedded().is_some()
}

struct File {
    bytes: &'static [u8],
    content_type: HeaderValue,
    etag: HeaderValue,
}

/// A built web client, keyed by URL path (`/index.html`, `/assets/app-1a2b.js`).
///
/// Public, as `WebBundle`, only so a router test can build one from fixture
/// files and hand it to `AppState::with_web_client_bundle`.
pub struct Bundle {
    files: HashMap<&'static str, File>,
}

impl Bundle {
    /// `None` when there is no `/index.html`, which is the empty table of a
    /// build without the feature.
    pub fn new(entries: &[(&'static str, &'static [u8])]) -> Option<Self> {
        let files: HashMap<_, _> = entries
            .iter()
            .map(|&(path, bytes)| {
                let file = File {
                    bytes,
                    content_type: HeaderValue::from_static(content_type(path)),
                    etag: etag(bytes),
                };
                (path, file)
            })
            .collect();
        files.contains_key("/index.html").then_some(Self { files })
    }

    /// Answer one request for a non-API path.
    pub(crate) fn respond(
        &self,
        method: &Method,
        path: &str,
        headers: &HeaderMap,
    ) -> Response<Body> {
        if method != Method::GET && method != Method::HEAD {
            return plain(StatusCode::METHOD_NOT_ALLOWED, |h| {
                h.insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
            });
        }
        let (file, cache) = if path.starts_with("/assets/") {
            match self.files.get(path) {
                Some(file) => (file, IMMUTABLE),
                None => {
                    return plain(StatusCode::NOT_FOUND, |h| {
                        h.insert(CACHE_CONTROL, NO_STORE);
                    })
                }
            }
        } else {
            let file = self
                .files
                .get(path)
                .unwrap_or_else(|| &self.files["/index.html"]);
            (file, REVALIDATE)
        };

        let not_modified = headers
            .get(IF_NONE_MATCH)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| etag_matches(value, &file.etag));
        let mut response = if not_modified {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::NOT_MODIFIED;
            response
        } else {
            let mut response = Response::new(Body::from(Bytes::from_static(file.bytes)));
            response
                .headers_mut()
                .insert(CONTENT_TYPE, file.content_type.clone());
            response
        };
        let h = response.headers_mut();
        h.insert(ETAG, file.etag.clone());
        h.insert(CACHE_CONTROL, cache);
        h.insert(X_CONTENT_TYPE_OPTIONS, NOSNIFF);
        response
    }
}

fn plain(status: StatusCode, extra: impl FnOnce(&mut HeaderMap)) -> Response<Body> {
    let reason = status.canonical_reason().unwrap_or_default();
    let mut response = Response::new(Body::from(reason));
    *response.status_mut() = status;
    let h = response.headers_mut();
    h.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    h.insert(X_CONTENT_TYPE_OPTIONS, NOSNIFF);
    extra(h);
    response
}

/// A validator from the content, so it survives a restart and agrees across
/// instances of the same build. Weak, because the compression layer may send
/// the same file under several content codings without touching this header,
/// and a strong validator promises byte-for-byte identity (RFC 9110 § 8.8.1).
fn etag(bytes: &[u8]) -> HeaderValue {
    let digest = Sha256::digest(bytes);
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    HeaderValue::from_str(&format!("W/\"{hex}\"")).expect("hex is a valid header value")
}

/// `If-None-Match` uses weak comparison (RFC 9110 § 13.1.2): a `W/` prefix is
/// ignored on both sides, the header may list several validators, and `*`
/// matches anything.
fn etag_matches(if_none_match: &str, etag: &HeaderValue) -> bool {
    let opaque = |tag: &str| tag.strip_prefix("W/").unwrap_or(tag).to_owned();
    let etag = opaque(etag.to_str().unwrap_or_default());
    if_none_match.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || opaque(candidate) == etag
    })
}

/// Types for what a Vite build emits. `nosniff` is set on every response, so a
/// script or stylesheet that fell through to the last arm would not load; add
/// the extension here when the bundle gains a new kind of file.
fn content_type(path: &str) -> &'static str {
    let extension = path.rsplit_once('.').map_or("", |(_, extension)| extension);
    match extension {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: &[u8] = b"<!doctype html><title>Axon</title>";
    const CHUNK: &[u8] = b"export const answer = 42";
    const VERSION: &[u8] = br#"{"version":"abc"}"#;

    fn bundle() -> Bundle {
        Bundle::new(&[
            ("/index.html", INDEX),
            ("/assets/app-1a2b.js", CHUNK),
            ("/version.json", VERSION),
        ])
        .expect("bundle with an index")
    }

    fn get(path: &str) -> Response<Body> {
        bundle().respond(&Method::GET, path, &HeaderMap::new())
    }

    async fn body(response: Response<Body>) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec()
    }

    fn header<'a>(response: &'a Response<Body>, name: &str) -> &'a str {
        response
            .headers()
            .get(name)
            .unwrap_or_else(|| panic!("missing {name}"))
            .to_str()
            .expect("ASCII header")
    }

    #[test]
    fn a_table_without_an_index_is_not_a_bundle() {
        assert!(Bundle::new(&[]).is_none());
        assert!(Bundle::new(&[("/assets/app-1a2b.js", CHUNK)]).is_none());
    }

    #[tokio::test]
    async fn the_root_and_client_routes_get_the_index_revalidated() {
        for path in [
            "/",
            "/index.html",
            "/rooms/abc",
            "/oauth/callback",
            "/nope.js",
        ] {
            let response = get(path);
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(
                header(&response, "content-type"),
                "text/html; charset=utf-8"
            );
            assert_eq!(header(&response, "cache-control"), "no-cache", "{path}");
            assert_eq!(body(response).await, INDEX, "{path}");
        }
    }

    #[tokio::test]
    async fn a_hashed_asset_is_immutable() {
        let response = get("/assets/app-1a2b.js");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header(&response, "content-type"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            header(&response, "cache-control"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(header(&response, "x-content-type-options"), "nosniff");
        assert_eq!(body(response).await, CHUNK);
    }

    /// The ADR 0087 trap: a chunk from a previous build must not come back as
    /// `index.html` with a `200`, and the miss must not be cached.
    #[tokio::test]
    async fn a_missing_asset_is_an_uncacheable_404() {
        for path in ["/assets/app-9z9z.js", "/assets/", "/assets/../index.html"] {
            let response = get(path);
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert_eq!(header(&response, "cache-control"), "no-store", "{path}");
            assert_ne!(body(response).await, INDEX, "{path}");
        }
    }

    #[tokio::test]
    async fn a_mutable_file_is_served_as_itself_and_revalidated() {
        let response = get("/version.json");
        assert_eq!(header(&response, "content-type"), "application/json");
        assert_eq!(header(&response, "cache-control"), "no-cache");
        assert_eq!(body(response).await, VERSION);
    }

    #[tokio::test]
    async fn a_matching_validator_gets_a_304_with_no_body() {
        let etag = header(&get("/version.json"), "etag").to_owned();
        // Weak: the compression layer serves the same file under several codings.
        let strong = etag.strip_prefix("W/").expect("a weak validator");
        for sent in [
            etag.clone(),
            strong.to_owned(),
            format!("\"other\", {etag}"),
            "*".to_owned(),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(IF_NONE_MATCH, sent.parse().expect("header value"));
            let response = bundle().respond(&Method::GET, "/version.json", &headers);
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{sent}");
            assert_eq!(header(&response, "etag"), etag);
            assert_eq!(header(&response, "cache-control"), "no-cache");
            assert!(body(response).await.is_empty());
        }
    }

    #[test]
    fn a_stale_validator_gets_the_file() {
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, HeaderValue::from_static("\"stale\""));
        let response = bundle().respond(&Method::GET, "/version.json", &headers);
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn different_content_gets_a_different_validator() {
        assert_ne!(etag(INDEX), etag(CHUNK));
        assert_eq!(etag(INDEX), etag(INDEX));
    }

    #[test]
    fn head_is_allowed_and_writes_are_not() {
        let head = bundle().respond(&Method::HEAD, "/", &HeaderMap::new());
        assert_eq!(head.status(), StatusCode::OK);
        let post = bundle().respond(&Method::POST, "/", &HeaderMap::new());
        assert_eq!(post.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(header(&post, "allow"), "GET, HEAD");
    }

    #[test]
    fn content_types_cover_what_vite_emits() {
        assert_eq!(content_type("/assets/app.css"), "text/css; charset=utf-8");
        assert_eq!(
            content_type("/manifest.webmanifest"),
            "application/manifest+json"
        );
        assert_eq!(content_type("/favicon.svg"), "image/svg+xml");
        assert_eq!(content_type("/icon-180.png"), "image/png");
        assert_eq!(content_type("/LICENSE"), "application/octet-stream");
    }
}
