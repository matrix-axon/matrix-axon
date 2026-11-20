//! The `axon-media` scheme (ADR 0139 § 7): a `<video>` or `<audio>` element
//! in the shell loads a stream ticket through this process, because the shell
//! CSP cannot list a self-hosted server in `media-src`, and a plain-http
//! server would be mixed content from the custom-scheme page.
//!
//! `axon-media://localhost/<ticket>` (or `http://axon-media.localhost/<ticket>`
//! on Windows and Android) becomes `<base>/v1/media-stream/<ticket>`, where the
//! base is whatever the page last passed to `set_media_stream_base`. The
//! handler reaches that prefix and nothing else: the path must be a bare
//! ticket, and a redirect is refused rather than followed. It sends no
//! credential of its own, since the ticket is one.
//!
//! Tauri's responder takes a whole body, not a stream, so each response is
//! bounded at [`MAX_BODY`]: a shorter `206` whose `Content-Range` says which
//! bytes it holds, after which the player asks for the next range.
//!
//! A plain single-range `GET` is answered from the read-ahead window
//! (`media_window`), which fetches the object in 4 MiB blocks on a fixed grid
//! and rewrites the outgoing `Range` to match. Everything else (the stall
//! probe, a conditional request, a `HEAD`, a multi-range or Range-less
//! request, and a suffix range before the object's length is known) goes
//! upstream as sent, and the bound is applied to what comes back.
//!
//! Tickets are credentials, so nothing here logs a URL or a path.

use std::sync::{PoisonError, RwLock};
use std::time::Duration;

use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode, Url};
use tauri::http::{Request, Response};
use tokio::time::Instant;

use crate::error::{Kind, TransportError};
use crate::media_window::{MediaWindows, Outcome, Spec};
use crate::transport::{Outgoing, Transport};

/// The scheme's name, which `convertFileSrc(ticket, 'axon-media')` produces.
pub const SCHEME: &str = "axon-media";

/// The most body one scheme response carries (ADR 0139 § 7). What keeps the
/// shell's memory per response fixed, since the responder holds it whole.
pub(crate) const MAX_BODY: usize = 4 << 20;

/// Once a response has this much body and [`EARLY_AFTER`] has passed since
/// its headers, it is returned as it stands rather than waiting for the rest
/// of [`MAX_BODY`]. Without it a slow link would hold the player's first frame
/// until 4 MiB had arrived: 16 s at 2 Mbit/s. A shorter `206` is valid, so the
/// player only asks for the next range sooner.
pub(crate) const EARLY_MIN: usize = 256 << 10;
pub(crate) const EARLY_AFTER: Duration = Duration::from_secs(1);

/// A backstop on one scheme request, headers and body together. The server
/// bounds its own waits (ADR 0139 § 3), so this only catches a link that has
/// gone silent; without it the task, and the upstream stream, would live on.
pub(crate) const DEADLINE: Duration = Duration::from_secs(120);

/// The request header the page's stall probe adds (ADR 0139 § 6), so that it
/// goes upstream rather than being answered from the read-ahead window, and
/// so sees an expired ticket as soon as the server does.
pub(crate) const PROBE_HEADER: &str = "axon-stream-probe";

/// How much of an error response's body is passed on. They are small JSON.
const MAX_ERROR_BODY: usize = 64 << 10;

/// Where every ticket path the server issues begins, relative to the base.
const STREAM_PREFIX: &str = "v1/media-stream/";

/// The longest path segment taken as a ticket. The server issues 43
/// base64url characters (32 bytes); anything far longer is not one.
const MAX_TICKET_LEN: usize = 256;

/// The server base the page last set, or `None` until it does.
#[derive(Default)]
pub struct MediaBase(RwLock<Option<Url>>);

impl MediaBase {
    pub(crate) fn get(&self) -> Option<Url> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replace the base. Refused unless it is an `http` or `https` URL with
    /// no credentials, query or fragment.
    pub(crate) fn set(&self, base: &str) -> Result<(), TransportError> {
        let url = parse_base(base).ok_or_else(|| TransportError::new(Kind::Scope))?;
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = Some(url);
        Ok(())
    }
}

/// The base as a URL whose path ends in `/`, so that joining the stream
/// prefix keeps whatever path the server is mounted under.
pub(crate) fn parse_base(base: &str) -> Option<Url> {
    let mut url = Url::parse(base).ok()?;
    let plain = matches!(url.scheme(), "http" | "https")
        && url.has_host()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none();
    if !plain {
        return None;
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Some(url)
}

/// The ticket a scheme path names, or `None` if the path is not exactly one
/// segment of base64url characters. The page's `convertFileSrc` percent-encodes
/// the ticket, and base64url needs no encoding, so a `%` is refused too.
pub(crate) fn ticket_of(path: &str) -> Option<&str> {
    let ticket = path.strip_prefix('/')?;
    let valid = !ticket.is_empty()
        && ticket.len() <= MAX_TICKET_LEN
        && ticket
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    valid.then_some(ticket)
}

/// The page's origin as a webview serialises it for CORS: `axon://localhost`
/// for the shell's own scheme on macOS, iOS and Linux, `http://axon.localhost`
/// on Windows and Android, and the dev server's in a dev build. The `url`
/// crate calls any non-special scheme's origin opaque (`null`), so those are
/// built from the scheme, host and port instead.
pub(crate) fn page_origin(url: &Url) -> Option<String> {
    if matches!(url.scheme(), "http" | "https") {
        return Some(url.origin().ascii_serialization());
    }
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    })
}

/// `<base>/v1/media-stream/<ticket>`.
pub(crate) fn upstream_url(base: &Url, ticket: &str) -> Option<Url> {
    base.join(STREAM_PREFIX).ok()?.join(ticket).ok()
}

/// A `Content-Range: bytes first-last/total` value, with `total` `None` for
/// `*`. `None` for anything else, including the `bytes */total` of a `416`.
pub(crate) fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (span, total) = rest.split_once('/')?;
    let (first, last) = span.split_once('-')?;
    let first: u64 = first.trim().parse().ok()?;
    let last: u64 = last.trim().parse().ok()?;
    let total = match total.trim() {
        "*" => None,
        total => Some(total.parse().ok()?),
    };
    (first <= last && total.is_none_or(|total| last < total)).then_some((first, last, total))
}

/// Why a body could not be read to the length the handler meant to return.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ShortBody {
    /// It ended first.
    Ended,
    /// It failed first: the server aborted it, or the link dropped.
    Failed,
}

/// Read `want` bytes of `response`'s body, or fewer once `early` is
/// `Some((min, at))` and at least `min` bytes have arrived by `at`. The
/// response is dropped afterwards, which ends the upstream stream.
async fn read_up_to(
    response: &mut reqwest::Response,
    want: usize,
    early: Option<(usize, Instant)>,
) -> Result<Vec<u8>, ShortBody> {
    let mut body = Vec::with_capacity(want.min(MAX_BODY));
    while body.len() < want {
        let chunk = match early {
            Some((min, at)) if body.len() >= min => {
                match tokio::time::timeout_at(at, response.chunk()).await {
                    Ok(chunk) => chunk,
                    Err(_) => break,
                }
            }
            _ => response.chunk().await,
        };
        match chunk {
            Ok(Some(bytes)) => {
                let take = bytes.len().min(want - body.len());
                body.extend_from_slice(&bytes[..take]);
            }
            Ok(None) => return Err(ShortBody::Ended),
            Err(_) => return Err(ShortBody::Failed),
        }
    }
    Ok(body)
}

/// The request headers the player may send upstream. The ticket is the only
/// credential, so nothing that could carry another goes: no cookies, no
/// `Authorization`, no `Origin` or `Referer`.
pub(crate) const FORWARDED_REQUEST: [HeaderName; 3] =
    [header::RANGE, header::IF_NONE_MATCH, header::IF_RANGE];

/// The response headers the player is given back, beside the `Content-Length`
/// and `Content-Range` the handler sets for the body it returns.
const FORWARDED_RESPONSE: [HeaderName; 4] = [
    header::CONTENT_TYPE,
    header::ACCEPT_RANGES,
    header::ETAG,
    header::CACHE_CONTROL,
];

/// What the scheme answers for one request. `origin` is the page's own, for
/// the CORS headers the section 6 probe needs: in the shells the probe is a
/// cross-origin `fetch` from the app's scheme to this one.
pub(crate) async fn respond(
    transport: &Transport,
    windows: &MediaWindows,
    base: Option<Url>,
    request: &Request<Vec<u8>>,
    origin: Option<&str>,
) -> Response<Vec<u8>> {
    let mut response = if request.method() == Method::OPTIONS {
        preflight()
    } else {
        match tokio::time::timeout(DEADLINE, proxy(transport, windows, base, request)).await {
            Ok(response) => response,
            Err(_) => status(StatusCode::GATEWAY_TIMEOUT),
        }
    };
    if let Some(origin) = origin.and_then(|origin| HeaderValue::from_str(origin).ok()) {
        allow_origin(response.headers_mut(), origin);
    }
    response
}

/// The CORS headers that let the page at `origin` read a response: the
/// section 6 probe is a cross-origin `fetch` in the shells.
pub(crate) fn allow_origin(headers: &mut HeaderMap, origin: HeaderValue) {
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("Content-Range, Accept-Ranges, ETag"),
    );
}

/// The probe's `Range` header is not CORS-safelisted everywhere, and it adds
/// [`PROBE_HEADER`], so its `fetch` is preflighted.
pub(crate) fn preflight() -> Response<Vec<u8>> {
    let mut response = status(StatusCode::NO_CONTENT);
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, HEAD"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Range, If-None-Match, If-Range, Axon-Stream-Probe"),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
    response
}

pub(crate) fn status(status: StatusCode) -> Response<Vec<u8>> {
    let mut response = Response::new(Vec::new());
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub(crate) fn bad_gateway() -> Response<Vec<u8>> {
    status(StatusCode::BAD_GATEWAY)
}

/// The response headers in [`FORWARDED_RESPONSE`] that `upstream` carries.
pub(crate) fn forwarded_response_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for name in FORWARDED_RESPONSE {
        if let Some(value) = upstream.get(&name) {
            headers.insert(name, value.clone());
        }
    }
    headers
}

/// The range of a request the read-ahead window answers: a plain `GET` of
/// one range. The probe, a conditional request and anything else go upstream
/// as sent.
fn window_spec(request: &Request<Vec<u8>>) -> Option<Spec> {
    let headers = request.headers();
    if request.method() != Method::GET
        || headers.contains_key(PROBE_HEADER)
        || headers.contains_key(header::IF_NONE_MATCH)
    {
        return None;
    }
    headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(Spec::parse)
}

/// After [`respond`] has answered `request`, fetch the window's next block if
/// the read calls for one ([`MediaWindows::read_ahead`]). The shell runs this
/// once the response is on its way, so it never delays one.
pub(crate) async fn read_ahead(
    transport: &Transport,
    windows: &MediaWindows,
    base: Option<Url>,
    request: &Request<Vec<u8>>,
) {
    let Some(spec) = window_spec(request) else {
        return;
    };
    let Some((ticket, url)) = base.and_then(|base| {
        let ticket = ticket_of(request.uri().path())?;
        Some((ticket, upstream_url(&base, ticket)?))
    }) else {
        return;
    };
    let _ = tokio::time::timeout(DEADLINE, windows.read_ahead(transport, url, ticket, spec)).await;
}

async fn proxy(
    transport: &Transport,
    windows: &MediaWindows,
    base: Option<Url>,
    request: &Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    let method = request.method().clone();
    if method != Method::GET && method != Method::HEAD {
        return status(StatusCode::METHOD_NOT_ALLOWED);
    }
    // Not yet told where the server is: nothing to reach.
    let Some(base) = base else {
        return status(StatusCode::SERVICE_UNAVAILABLE);
    };
    // The server's own answer for a ticket it does not know, so the page's
    // probe treats a malformed one the same way.
    let Some((ticket, url)) = ticket_of(request.uri().path())
        .and_then(|ticket| Some((ticket, upstream_url(&base, ticket)?)))
    else {
        return status(StatusCode::NOT_FOUND);
    };

    if let Some(spec) = window_spec(request) {
        if let Outcome::Served(response) = windows.serve(transport, url.clone(), ticket, spec).await
        {
            return response;
        }
    }

    let mut headers = HeaderMap::new();
    for name in FORWARDED_REQUEST {
        for value in request.headers().get_all(&name) {
            headers.append(name.clone(), value.clone());
        }
    }
    // Media is already compressed, and a decoded body would lose the
    // `Content-Length` and `Content-Range` the bound is computed from.
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    let ranged = headers.contains_key(header::RANGE);

    match transport
        .send_once(Outgoing {
            method: method.clone(),
            url,
            headers,
            body: None,
        })
        .await
    {
        Ok(upstream) => shape(&method, ranged, upstream).await,
        Err(_) => bad_gateway(),
    }
}

/// The scheme's answer for an upstream response, its body bounded at
/// [`MAX_BODY`] (ADR 0139 § 7).
pub(crate) async fn shape(
    method: &Method,
    ranged: bool,
    mut upstream: reqwest::Response,
) -> Response<Vec<u8>> {
    let code = upstream.status();
    let mut response = Response::new(Vec::new());
    *response.status_mut() = code;
    *response.headers_mut() = forwarded_response_headers(upstream.headers());
    let length = upstream
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    if *method == Method::HEAD || code == StatusCode::NOT_MODIFIED {
        let headers = response.headers_mut();
        if let Some(length) = length {
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
        }
        // A `206` to a ranged `HEAD` must say which bytes it describes.
        if let Some(range) = upstream.headers().get(header::CONTENT_RANGE) {
            headers.insert(header::CONTENT_RANGE, range.clone());
        }
        return response;
    }
    match code {
        StatusCode::PARTIAL_CONTENT => {
            let Some((first, last, total)) = upstream
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range)
            else {
                return bad_gateway();
            };
            // `first <= last`, so only the `+ 1` could overflow, for a span
            // the peer states as the whole of `u64`.
            let want = usize::try_from(last - first)
                .map_or(MAX_BODY, |span| span.saturating_add(1).min(MAX_BODY));
            let early = Some((EARLY_MIN, Instant::now() + EARLY_AFTER));
            let body = match read_up_to(&mut upstream, want, early).await {
                Ok(body) if !body.is_empty() => body,
                _ => return bad_gateway(),
            };
            let last = first + (body.len() as u64 - 1);
            let total = total.map_or_else(|| "*".to_owned(), |total| total.to_string());
            let range = format!("bytes {first}-{last}/{total}");
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&range).expect("digits and ASCII"),
            );
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
            *response.body_mut() = body;
            response
        }
        StatusCode::OK => {
            // A `206` cannot answer a request without `Range`, and a whole
            // object over the bound cannot be returned, so this is refused
            // rather than truncated (ADR 0139 § 7).
            let fits = length.is_some_and(|length| length <= MAX_BODY as u64);
            if !fits {
                eprintln!(
                    "axon-media: refused a {} {} response over {} bytes on {}",
                    if ranged {
                        "ranged request's"
                    } else {
                        "Range-less request's"
                    },
                    code.as_u16(),
                    MAX_BODY,
                    std::env::consts::OS,
                );
                return bad_gateway();
            }
            let want = usize::try_from(length.unwrap_or(0)).unwrap_or(usize::MAX);
            let Ok(body) = read_up_to(&mut upstream, want, None).await else {
                return bad_gateway();
            };
            response
                .headers_mut()
                .insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
            *response.body_mut() = body;
            response
        }
        // Never followed, and never handed to the player: a `Location` would
        // let it load something other than the ticket route.
        code if code.is_redirection() => bad_gateway(),
        _ => {
            // `416` says where the object ends.
            if let Some(range) = upstream.headers().get(header::CONTENT_RANGE) {
                response
                    .headers_mut()
                    .insert(header::CONTENT_RANGE, range.clone());
            }
            let want = length.map_or(MAX_ERROR_BODY, |length| {
                usize::try_from(length)
                    .unwrap_or(usize::MAX)
                    .min(MAX_ERROR_BODY)
            });
            // The status is what matters to the page; a body that does not
            // arrive whole is dropped rather than failing the answer.
            let body = match read_up_to(&mut upstream, want, None).await {
                Ok(body) if length.is_some() => body,
                _ => Vec::new(),
            };
            response
                .headers_mut()
                .insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
            *response.body_mut() = body;
            response
        }
    }
}
