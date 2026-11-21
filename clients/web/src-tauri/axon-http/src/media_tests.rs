//! The `axon-media` handler against an in-process server that answers ticket
//! requests the way Axon's ticket route does (ADR 0139 §§ 3, 5 and 7).

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use hyper::body::{Body as HttpBody, Bytes, Frame, Incoming};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use reqwest::header::{self, HeaderMap};
use reqwest::{StatusCode, Url};
use tauri::http::{Request, Response};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::client::ClientOptions;
use crate::media::{
    self, page_origin, parse_base, parse_content_range, ticket_of, upstream_url, MAX_BODY,
};
use crate::media_window::{block_start, MediaWindows, Spec, MAX_BLOCKS};
use crate::transport::Transport;

const ORIGIN: &str = "axon://localhost";
/// Over twice the bound, and not a multiple of it.
const LARGE: usize = 9 * 1024 * 1024 + 123;
const SMALL: usize = 1000;
const TICKET: &str = "AAAAbbbbCCCCddddEEEEffffGGGGhhhhIIIIjjjjKK0";

fn object(len: usize) -> Vec<u8> {
    pattern()[..len].to_vec()
}

/// The bytes every object is a prefix of, built once: rebuilding 9 MiB for
/// every assertion made the per-sample test take most of a minute.
fn pattern() -> &'static [u8] {
    static PATTERN: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| (0..LARGE).map(|i| (i % 251) as u8).collect())
}

// ===== server =====

/// A body fed from a channel, so a test can end it with an error, or hold it.
struct ChannelBody(mpsc::Receiver<Result<Bytes, std::io::Error>>);

impl HttpBody for ChannelBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.0
            .poll_recv(cx)
            .map(|next| next.map(|item| item.map(Frame::data)))
    }
}

/// Feed `chunks` into a body, then `end` it: `Ok` closes, `Err` aborts, and
/// `None` holds it open for good.
fn body(chunks: Vec<Bytes>, end: Option<Result<(), ()>>) -> ChannelBody {
    let (tx, rx) = mpsc::channel(4);
    tokio::spawn(async move {
        for chunk in chunks {
            if tx.send(Ok(chunk)).await.is_err() {
                return;
            }
        }
        match end {
            Some(Ok(())) => {}
            Some(Err(())) => {
                let _ = tx
                    .send(Err(std::io::Error::other("withheld tail refused")))
                    .await;
            }
            None => {
                tx.closed().await;
            }
        }
    });
    ChannelBody(rx)
}

/// Feed `chunks` into a body `gap` apart, then close it: a slow but steady
/// link.
fn paced(chunks: Vec<Bytes>, gap: Duration) -> ChannelBody {
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        for chunk in chunks {
            tokio::time::sleep(gap).await;
            if tx.send(Ok(chunk)).await.is_err() {
                return;
            }
        }
    });
    ChannelBody(rx)
}

/// Split a body into 64 KiB chunks, as a streamed response arrives.
fn chunks(bytes: &[u8]) -> Vec<Bytes> {
    bytes.chunks(64 << 10).map(Bytes::copy_from_slice).collect()
}

struct Server {
    addr: std::net::SocketAddr,
    /// The path and headers of every request the server received.
    seen: Arc<Mutex<Vec<(String, HeaderMap)>>>,
}

impl Server {
    fn base(&self) -> Url {
        parse_base(&format!("http://{}/axon", self.addr)).unwrap()
    }

    fn seen(&self) -> Vec<(String, HeaderMap)> {
        self.seen.lock().unwrap().clone()
    }
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` against `len`, as the server
/// resolves them; `Err` for one past the end.
fn resolve(range: &str, len: usize) -> Result<(usize, usize), ()> {
    let spec = range.strip_prefix("bytes=").ok_or(())?;
    let (a, b) = spec.split_once('-').ok_or(())?;
    let (first, last) = match (a, b) {
        ("", n) => {
            let n: usize = n.parse().map_err(|_| ())?;
            (len.saturating_sub(n), len - 1)
        }
        (a, "") => (a.parse().map_err(|_| ())?, len - 1),
        (a, b) => (
            a.parse().map_err(|_| ())?,
            b.parse::<usize>().map_err(|_| ())?.min(len - 1),
        ),
    };
    if first >= len || first > last {
        return Err(());
    }
    Ok((first, last))
}

/// What the ticket route does for `ticket`: most act as a valid ticket for
/// an object of `LARGE` bytes, and a few names stand for the cases a test
/// needs.
fn answer(ticket: &str, headers: &HeaderMap) -> hyper::Response<ChannelBody> {
    let builder = hyper::Response::builder()
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, "\"e1\"")
        .header(header::CONTENT_TYPE, "video/mp4");
    let len = if ticket == "small" { SMALL } else { LARGE };
    let whole = object(len);
    if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|v| v == "\"e1\"")
    {
        return builder
            .status(304)
            .body(body(Vec::new(), Some(Ok(()))))
            .unwrap();
    }
    match ticket {
        "gone" => {
            let json = Bytes::from_static(br#"{"error":"not_found"}"#);
            return hyper::Response::builder()
                .status(404)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, json.len())
                .body(body(vec![json], Some(Ok(()))))
                .unwrap();
        }
        "moved" => {
            return hyper::Response::builder()
                .status(302)
                .header(header::LOCATION, "http://elsewhere.invalid/")
                .body(body(Vec::new(), Some(Ok(()))))
                .unwrap();
        }
        _ => {}
    }
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .filter(|_| ticket != "ignores-range");
    let Some(range) = range else {
        return builder
            .status(200)
            .header(header::CONTENT_LENGTH, len)
            .body(body(chunks(&whole), Some(Ok(()))))
            .unwrap();
    };
    if ticket == "boundless" {
        // A peer stating a span as wide as `u64`, then sending 50 bytes.
        let first = range
            .strip_prefix("bytes=")
            .and_then(|spec| spec.split('-').next())
            .and_then(|first| first.parse::<u64>().ok())
            .unwrap_or(0);
        return builder
            .status(206)
            .header(
                header::CONTENT_RANGE,
                format!("bytes {first}-{}/*", u64::MAX),
            )
            .body(body(vec![Bytes::from(vec![7u8; 50])], Some(Ok(()))))
            .unwrap();
    }
    let Ok((first, mut last)) = resolve(range, len) else {
        return builder
            .status(416)
            .header(header::CONTENT_RANGE, format!("bytes */{len}"))
            .header(header::CONTENT_LENGTH, 0)
            .body(body(Vec::new(), Some(Ok(()))))
            .unwrap();
    };
    // A server that ignores the end asked for and sends to the object's end.
    if ticket == "wide" {
        last = len - 1;
    }
    let slice = &whole[first..=last];
    let builder = builder
        .status(206)
        .header(header::CONTENT_RANGE, format!("bytes {first}-{last}/{len}"))
        .header(header::CONTENT_LENGTH, slice.len());
    // The whole slice, 256 KiB every 150 ms.
    if ticket == "trickle" {
        let sent = slice
            .chunks(256 << 10)
            .map(Bytes::copy_from_slice)
            .collect();
        return builder
            .body(paced(sent, Duration::from_millis(150)))
            .unwrap();
    }
    let (sent, end) = match ticket {
        // The server's abort at the withheld tail (ADR 0139 § 3): some bytes,
        // then the body fails.
        "tampered" => (chunks(&slice[..slice.len().min(512 << 10)]), Some(Err(()))),
        // A download still running behind a slow homeserver: 320 KiB, then
        // nothing more for now. `slow1`, `slow2` and so on are more of them.
        t if t.starts_with("slow") => (chunks(&slice[..slice.len().min(320 << 10)]), None),
        // Ends cleanly but short of its own Content-Length.
        "short" => (chunks(&slice[..slice.len() / 2]), Some(Ok(()))),
        _ => (chunks(slice), Some(Ok(()))),
    };
    builder.body(body(sent, end)).unwrap()
}

async fn server() -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let service = service_fn(move |request: hyper::Request<Incoming>| {
                    let log = Arc::clone(&log);
                    async move {
                        let path = request.uri().path().to_owned();
                        log.lock()
                            .unwrap()
                            .push((path.clone(), request.headers().clone()));
                        let ticket = path
                            .strip_prefix("/axon/v1/media-stream/")
                            .unwrap_or("unknown");
                        // A server slow to answer at all.
                        if ticket == "late" {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                        Ok::<_, Infallible>(answer(ticket, request.headers()))
                    }
                });
                let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Server { addr, seen }
}

// ===== client side =====

fn transport() -> Transport {
    Transport::new(ClientOptions::new("Axon/test (test)".into())).unwrap()
}

fn scheme_request(method: &str, ticket: &str, headers: &[(&str, &str)]) -> Request<Vec<u8>> {
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("axon-media://localhost/{ticket}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Vec::new()).unwrap()
}

async fn get(server: &Server, ticket: &str, headers: &[(&str, &str)]) -> Response<Vec<u8>> {
    let request = scheme_request("GET", ticket, headers);
    media::respond(
        &transport(),
        &MediaWindows::default(),
        Some(server.base()),
        &request,
        Some(ORIGIN),
    )
    .await
}

fn header_of(response: &Response<Vec<u8>>, name: header::HeaderName) -> &str {
    response
        .headers()
        .get(name)
        .map(|v| v.to_str().unwrap())
        .unwrap_or("")
}

/// The status, `Content-Range`, `Content-Length` and the body's length agree,
/// and the body holds the object's bytes at that range.
fn assert_partial(response: &Response<Vec<u8>>, first: usize, len: usize, total: usize) {
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        header_of(response, header::CONTENT_RANGE),
        format!("bytes {first}-{}/{total}", first + len - 1)
    );
    assert_eq!(header_of(response, header::CONTENT_LENGTH), len.to_string());
    assert_eq!(response.body().len(), len);
    assert!(
        response.body()[..] == pattern()[first..first + len],
        "the body is not the object's bytes at {first}"
    );
}

// ===== pure parts =====

#[test]
fn a_base_keeps_its_path_and_refuses_anything_but_plain_http() {
    assert_eq!(
        parse_base("https://h.example/axon").unwrap().as_str(),
        "https://h.example/axon/"
    );
    assert_eq!(
        parse_base("http://192.168.1.4:8080").unwrap().as_str(),
        "http://192.168.1.4:8080/"
    );
    for refused in [
        "ftp://h.example/",
        "file:///etc/",
        "https://user:pw@h.example/",
        "https://h.example/?q=1",
        "https://h.example/#f",
        "not a url",
        "tauri://localhost/",
    ] {
        assert!(parse_base(refused).is_none(), "{refused}");
    }
}

#[test]
fn only_a_bare_base64url_segment_is_a_ticket() {
    assert_eq!(ticket_of(&format!("/{TICKET}")), Some(TICKET));
    for refused in ["/", "", "/a/b", "/%2e%2e", "/..", "/a.b", "/a%2Fb", "/a?b"] {
        assert_eq!(ticket_of(refused), None, "{refused}");
    }
    assert_eq!(ticket_of(&format!("/{}", "a".repeat(257))), None);
}

#[test]
fn the_page_origin_is_the_one_the_webview_sends() {
    for (page, origin) in [
        ("axon://localhost/", "axon://localhost"),
        ("axon://localhost/@a:b/rooms/!c:d", "axon://localhost"),
        ("http://axon.localhost/rooms", "http://axon.localhost"),
        ("http://localhost:5173/index.html", "http://localhost:5173"),
    ] {
        assert_eq!(
            page_origin(&Url::parse(page).unwrap()).as_deref(),
            Some(origin),
            "{page}"
        );
    }
}

#[test]
fn the_upstream_url_is_the_ticket_route_under_the_base() {
    let base = parse_base("https://h.example/axon").unwrap();
    assert_eq!(
        upstream_url(&base, TICKET).unwrap().as_str(),
        format!("https://h.example/axon/v1/media-stream/{TICKET}")
    );
}

#[test]
fn content_ranges_parse_only_when_well_formed() {
    assert_eq!(parse_content_range("bytes 0-1/10"), Some((0, 1, Some(10))));
    assert_eq!(parse_content_range("bytes 5-9/*"), Some((5, 9, None)));
    for refused in [
        "bytes */10",
        "bytes 5-4/10",
        "bytes 0-10/10",
        "items 0-1/2",
        "",
    ] {
        assert_eq!(parse_content_range(refused), None, "{refused}");
    }
}

// ===== against the server =====

#[tokio::test]
async fn a_small_closed_range_comes_back_whole() {
    let server = server().await;
    let response = get(&server, TICKET, &[("range", "bytes=0-1")]).await;
    assert_partial(&response, 0, 2, LARGE);
    assert_eq!(header_of(&response, header::CONTENT_TYPE), "video/mp4");
    assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-store");
    assert_eq!(header_of(&response, header::ACCEPT_RANGES), "bytes");
}

#[tokio::test]
async fn an_absolute_range_goes_upstream_as_one_window_and_nothing_else_does() {
    let server = server().await;
    let request = scheme_request(
        "GET",
        TICKET,
        &[
            ("range", "bytes=0-"),
            ("cookie", "a=b"),
            ("authorization", "Bearer t"),
            ("origin", ORIGIN),
            ("referer", "axon://localhost/rooms"),
            ("x-custom", "1"),
        ],
    );
    media::respond(
        &transport(),
        &MediaWindows::default(),
        Some(server.base()),
        &request,
        Some(ORIGIN),
    )
    .await;
    let seen = server.seen();
    assert_eq!(seen.len(), 1);
    let (path, headers) = &seen[0];
    assert_eq!(path, &format!("/axon/v1/media-stream/{TICKET}"));
    assert_eq!(
        headers.get(header::RANGE).unwrap(),
        &format!("bytes=0-{}", MAX_BODY - 1)
    );
    assert_eq!(headers.get(header::ACCEPT_ENCODING).unwrap(), "identity");
    for absent in ["cookie", "authorization", "origin", "referer", "x-custom"] {
        assert!(headers.get(absent).is_none(), "{absent} was forwarded");
    }
}

#[tokio::test]
async fn an_open_range_ends_where_its_block_does() {
    let server = server().await;
    let response = get(&server, TICKET, &[("range", "bytes=1000-")]).await;
    assert_partial(&response, 1000, MAX_BODY - 1000, LARGE);
}

#[tokio::test]
async fn a_closed_range_wider_than_the_bound_is_cut_too() {
    let server = server().await;
    let response = get(&server, TICKET, &[("range", "bytes=0-8388607")]).await;
    assert_partial(&response, 0, MAX_BODY, LARGE);
}

#[tokio::test]
async fn a_suffix_range_starts_where_the_server_says() {
    let server = server().await;
    let small = get(&server, TICKET, &[("range", "bytes=-100")]).await;
    assert_partial(&small, LARGE - 100, 100, LARGE);
    let wide = get(&server, TICKET, &[("range", "bytes=-6000000")]).await;
    assert_partial(&wide, LARGE - 6_000_000, MAX_BODY, LARGE);
}

#[tokio::test]
async fn a_range_past_the_end_passes_the_416_through() {
    let server = server().await;
    let response = get(&server, TICKET, &[("range", &format!("bytes={LARGE}-"))]).await;
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        header_of(&response, header::CONTENT_RANGE),
        format!("bytes */{LARGE}")
    );
    assert!(response.body().is_empty());
}

#[tokio::test]
async fn without_range_a_small_object_comes_back_whole_and_a_large_one_is_refused() {
    let server = server().await;
    let small = get(&server, "small", &[]).await;
    assert_eq!(small.status(), StatusCode::OK);
    assert_eq!(small.body(), &object(SMALL));
    assert_eq!(header_of(&small, header::CONTENT_LENGTH), SMALL.to_string());

    let large = get(&server, TICKET, &[]).await;
    assert_eq!(large.status(), StatusCode::BAD_GATEWAY);
    assert!(large.body().is_empty());
}

#[tokio::test]
async fn a_200_for_a_range_the_server_ignored_is_refused_when_large() {
    let server = server().await;
    let response = get(&server, "ignores-range", &[("range", "bytes=0-1")]).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn a_body_that_fails_or_ends_early_is_a_502_never_a_short_206() {
    let server = server().await;
    // Within the bound, so that every byte the server fails to send is one
    // the handler meant to return.
    for ticket in ["tampered", "short"] {
        let response = get(&server, ticket, &[("range", "bytes=0-3145727")]).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{ticket}");
        assert!(response.body().is_empty(), "{ticket}");
        assert!(
            response.headers().get(header::CONTENT_RANGE).is_none(),
            "{ticket}"
        );
    }
}

#[tokio::test]
async fn a_slow_body_returns_what_has_arrived_once_there_is_enough() {
    let server = server().await;
    let started = Instant::now();
    let response = get(&server, "slow", &[("range", "bytes=0-")]).await;
    let waited = started.elapsed();
    assert_partial(&response, 0, 320 << 10, LARGE);
    assert!(
        waited >= media::EARLY_AFTER && waited < media::EARLY_AFTER + Duration::from_secs(2),
        "returned after {waited:?}"
    );
}

#[tokio::test]
async fn an_expired_ticket_reaches_the_page_as_the_servers_404() {
    let server = server().await;
    let response = get(&server, "gone", &[("range", "bytes=0-0")]).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.body(), br#"{"error":"not_found"}"#);
    // The probe is a cross-origin fetch, so it can read the status only with
    // these.
    assert_eq!(
        header_of(&response, header::ACCESS_CONTROL_ALLOW_ORIGIN),
        ORIGIN
    );
    assert!(header_of(&response, header::ACCESS_CONTROL_EXPOSE_HEADERS).contains("Content-Range"));
}

#[tokio::test]
async fn a_redirect_is_refused_rather_than_followed_or_passed_on() {
    let server = server().await;
    let response = get(&server, "moved", &[("range", "bytes=0-")]).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(response.headers().get(header::LOCATION).is_none());
    assert_eq!(server.seen().len(), 1);
}

#[tokio::test]
async fn a_matching_etag_gets_the_servers_304() {
    let server = server().await;
    let response = get(
        &server,
        TICKET,
        &[("range", "bytes=0-"), ("if-none-match", "\"e1\"")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert!(response.body().is_empty());
    let (_, headers) = &server.seen()[0];
    assert_eq!(headers.get(header::IF_NONE_MATCH).unwrap(), "\"e1\"");
}

#[tokio::test]
async fn the_probe_preflight_is_answered_here() {
    let request = scheme_request(
        "OPTIONS",
        TICKET,
        &[
            ("origin", ORIGIN),
            ("access-control-request-method", "GET"),
            ("access-control-request-headers", "range"),
        ],
    );
    // No base: a preflight never goes upstream.
    let response = media::respond(
        &transport(),
        &MediaWindows::default(),
        None,
        &request,
        Some(ORIGIN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        header_of(&response, header::ACCESS_CONTROL_ALLOW_ORIGIN),
        ORIGIN
    );
    assert!(header_of(&response, header::ACCESS_CONTROL_ALLOW_HEADERS).contains("Range"));
    assert!(header_of(&response, header::ACCESS_CONTROL_ALLOW_METHODS).contains("GET"));
}

#[tokio::test]
async fn nothing_goes_upstream_without_a_base_a_ticket_or_a_read_method() {
    let server = server().await;
    let transport = transport();
    let no_base = media::respond(
        &transport,
        &MediaWindows::default(),
        None,
        &scheme_request("GET", TICKET, &[]),
        Some(ORIGIN),
    )
    .await;
    assert_eq!(no_base.status(), StatusCode::SERVICE_UNAVAILABLE);

    for path in ["", "a/b", "%2e%2e", "..%2Fv1%2Frooms"] {
        let response = media::respond(
            &transport,
            &MediaWindows::default(),
            Some(server.base()),
            &scheme_request("GET", path, &[]),
            Some(ORIGIN),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        // Still readable by the probe.
        assert_eq!(
            header_of(&response, header::ACCESS_CONTROL_ALLOW_ORIGIN),
            ORIGIN
        );
    }

    let post = media::respond(
        &transport,
        &MediaWindows::default(),
        Some(server.base()),
        &scheme_request("POST", TICKET, &[]),
        Some(ORIGIN),
    )
    .await;
    assert_eq!(post.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(server.seen().is_empty());
}

#[tokio::test]
async fn an_unreachable_server_is_a_502() {
    // Bound and not listening: refused on Linux, a timeout on macOS, so a
    // port that was just released is used instead.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let base = parse_base(&format!("http://{addr}/")).unwrap();
    let response = media::respond(
        &transport(),
        &MediaWindows::default(),
        Some(base),
        &scheme_request("GET", TICKET, &[("range", "bytes=0-")]),
        Some(ORIGIN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

// ===== the read-ahead window =====

/// One shell's state across requests: the transport, the window and the base.
struct Player {
    transport: Transport,
    windows: MediaWindows,
    base: Url,
}

impl Player {
    fn new(server: &Server) -> Arc<Self> {
        Self::with_windows(server, MediaWindows::default())
    }

    fn with_windows(server: &Server, windows: MediaWindows) -> Arc<Self> {
        Arc::new(Self {
            transport: transport(),
            windows,
            base: server.base(),
        })
    }

    async fn get(&self, ticket: &str, headers: &[(&str, &str)]) -> Response<Vec<u8>> {
        let request = scheme_request("GET", ticket, headers);
        media::respond(
            &self.transport,
            &self.windows,
            Some(self.base.clone()),
            &request,
            Some(ORIGIN),
        )
        .await
    }

    async fn range(&self, ticket: &str, range: &str) -> Response<Vec<u8>> {
        self.get(ticket, &[("range", range)]).await
    }

    /// `range`, then the read-ahead the shell runs once it has answered.
    async fn play(&self, ticket: &str, headers: &[(&str, &str)]) -> Response<Vec<u8>> {
        let request = scheme_request("GET", ticket, headers);
        let base = Some(self.base.clone());
        let response = media::respond(
            &self.transport,
            &self.windows,
            base.clone(),
            &request,
            Some(ORIGIN),
        )
        .await;
        media::read_ahead(&self.transport, &self.windows, base, &request).await;
        response
    }
}

const BLOCK: usize = MAX_BODY;

/// The `Range` of every request the server received, in order.
fn upstream_ranges(server: &Server) -> Vec<String> {
    server
        .seen()
        .iter()
        .map(|(_, headers)| {
            headers
                .get(header::RANGE)
                .map_or("(none)".to_owned(), |v| v.to_str().unwrap().to_owned())
        })
        .collect()
}

#[test]
fn only_a_single_byte_range_is_windowed() {
    assert_eq!(Spec::parse("bytes=0-1"), Some(Spec::Closed(0, 1)));
    assert_eq!(Spec::parse(" bytes=10- "), Some(Spec::From(10)));
    assert_eq!(Spec::parse("bytes=-500"), Some(Spec::Suffix(500)));
    for refused in [
        "bytes=0-1,5-9",
        "bytes=5-4",
        "bytes=-",
        "bytes=-0",
        "bytes=+1-2",
        "items=0-1",
        "bytes=a-b",
    ] {
        assert_eq!(Spec::parse(refused), None, "{refused}");
    }
}

#[tokio::test]
async fn per_sample_reads_share_one_upstream_request() {
    // AVFoundation's pattern through a custom scheme: one small closed range
    // per sample, front to back.
    let server = server().await;
    let player = Player::new(&server);
    let mut at = 0;
    for i in 0..300 {
        let len = if i % 2 == 0 { 17_000 } else { 650 };
        let response = player
            .range(TICKET, &format!("bytes={at}-{}", at + len - 1))
            .await;
        assert_partial(&response, at, len, LARGE);
        at += len;
    }
    assert_eq!(
        upstream_ranges(&server),
        vec![format!("bytes=0-{}", MAX_BODY - 1)]
    );
}

#[tokio::test]
async fn requests_that_arrive_together_share_one_fetch() {
    let server = server().await;
    let player = Player::new(&server);
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..20usize {
        let player = Arc::clone(&player);
        tasks.spawn(async move {
            let first = i * 100_000;
            let response = player
                .range(TICKET, &format!("bytes={first}-{}", first + 9_999))
                .await;
            assert_partial(&response, first, 10_000, LARGE);
        });
    }
    while let Some(done) = tasks.join_next().await {
        done.unwrap();
    }
    assert_eq!(server.seen().len(), 1);
}

#[test]
fn blocks_sit_on_a_grid_of_the_bound() {
    let bound = MAX_BODY as u64;
    assert_eq!(block_start(0), 0);
    assert_eq!(block_start(bound - 1), 0);
    assert_eq!(block_start(bound), bound);
    assert_eq!(block_start(3 * bound + 17), 3 * bound);
}

#[tokio::test]
async fn ranges_starting_just_before_one_another_share_a_block() {
    // AVFoundation's audio and video reads at 14:34:40 on production: three
    // ranges a few KiB apart, which each fetched a block of their own.
    let server = server().await;
    let player = Player::new(&server);
    for first in [4_482_858, 4_475_238, 4_473_738, 4_315_870] {
        assert_partial(
            &player
                .range(TICKET, &format!("bytes={first}-{}", first + 9_999))
                .await,
            first,
            10_000,
            LARGE,
        );
    }
    assert_eq!(
        upstream_ranges(&server),
        vec![format!("bytes={BLOCK}-{}", 2 * BLOCK - 1)]
    );
}

#[tokio::test]
async fn a_range_across_a_blocks_end_gets_the_blocks_part() {
    let server = server().await;
    let player = Player::new(&server);
    let first = BLOCK - 100;
    assert_partial(
        &player
            .range(TICKET, &format!("bytes={first}-{}", first + 999))
            .await,
        first,
        100,
        LARGE,
    );
    assert_eq!(server.seen().len(), 1);
}

#[tokio::test]
async fn a_range_past_the_window_fetches_the_block_that_holds_it() {
    let server = server().await;
    let player = Player::new(&server);
    assert_partial(&player.range(TICKET, "bytes=0-1").await, 0, 2, LARGE);
    let next = MAX_BODY + 10;
    assert_partial(
        &player
            .range(TICKET, &format!("bytes={next}-{}", next + 99))
            .await,
        next,
        100,
        LARGE,
    );
    // Back inside the first: no third request.
    assert_partial(
        &player.range(TICKET, "bytes=500-599").await,
        500,
        100,
        LARGE,
    );
    assert_eq!(
        upstream_ranges(&server),
        vec![
            format!("bytes=0-{}", MAX_BODY - 1),
            format!("bytes={BLOCK}-{}", 2 * BLOCK - 1),
        ]
    );
}

#[tokio::test]
async fn the_last_window_ends_where_the_object_does() {
    let server = server().await;
    let player = Player::new(&server);
    let near = LARGE - 1000;
    // A closed range reaching past the end gets what there is.
    assert_partial(
        &player
            .range(TICKET, &format!("bytes={near}-{}", LARGE + 50))
            .await,
        near,
        1000,
        LARGE,
    );
    // The length is known now, so a suffix inside that window needs no
    // request of its own.
    assert_partial(
        &player.range(TICKET, "bytes=-100").await,
        LARGE - 100,
        100,
        LARGE,
    );
    assert_eq!(server.seen().len(), 1);
}

#[tokio::test]
async fn the_probe_always_goes_upstream() {
    let server = server().await;
    let player = Player::new(&server);
    assert_partial(&player.range(TICKET, "bytes=0-1").await, 0, 2, LARGE);
    let probe = player
        .get(
            TICKET,
            &[("range", "bytes=0-0"), ("axon-stream-probe", "1")],
        )
        .await;
    assert_partial(&probe, 0, 1, LARGE);
    assert_eq!(
        upstream_ranges(&server),
        vec![format!("bytes=0-{}", MAX_BODY - 1), "bytes=0-0".to_owned()]
    );
}

#[tokio::test]
async fn an_expired_ticket_reaches_every_waiting_request_as_a_404() {
    let server = server().await;
    let player = Player::new(&server);
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..5u64 {
        let player = Arc::clone(&player);
        tasks.spawn(async move {
            let response = player.range("gone", &format!("bytes={i}-{i}")).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        });
    }
    while let Some(done) = tasks.join_next().await {
        done.unwrap();
    }
    assert_eq!(player.windows.len(), 0, "a refused fetch is not held");
}

#[tokio::test]
async fn a_fetch_abandoned_before_its_headers_strands_no_one() {
    let server = server().await;
    let player = Player::new(&server);
    // The request that claimed the block goes away while the server has yet
    // to answer, as when the scheme's deadline drops it.
    let claimer = {
        let player = Arc::clone(&player);
        tokio::spawn(async move { player.range("late", "bytes=0-99").await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    claimer.abort();
    let _ = claimer.await;
    // The next request does not wait on a fetch nobody is making.
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        player.range("late", "bytes=100-199"),
    )
    .await
    .expect("the request waited on the abandoned fetch");
    assert_partial(&response, 100, 100, LARGE);
}

#[tokio::test]
async fn a_window_that_fails_serves_what_arrived_and_502s_the_rest() {
    let server = server().await;
    let player = Player::new(&server);
    // "tampered" sends 512 KiB, then fails.
    assert_partial(&player.range("tampered", "bytes=0-99").await, 0, 100, LARGE);
    // Wider than what any "tampered" response holds: it fails whether it
    // waits on the first window or, if that has already failed, starts its
    // own.
    let past = player.range("tampered", "bytes=100-700099").await;
    assert_eq!(past.status(), StatusCode::BAD_GATEWAY);
    // A failed window is not reused: the next request asks again.
    let before = server.seen().len();
    let again = player.range("tampered", "bytes=0-99").await;
    assert_partial(&again, 0, 100, LARGE);
    assert_eq!(server.seen().len(), before + 1);
}

#[tokio::test]
async fn at_most_a_fixed_number_of_windows_are_held() {
    let server = server().await;
    let player = Player::new(&server);
    for i in 0..(MAX_BLOCKS + 4) {
        let ticket = format!("t{i}");
        assert_partial(&player.range(&ticket, "bytes=0-1").await, 0, 2, LARGE);
    }
    assert_eq!(player.windows.len(), MAX_BLOCKS);
    player.windows.clear();
    assert_eq!(player.windows.len(), 0);
}

// ===== reading ahead =====

#[tokio::test]
async fn a_read_from_a_complete_block_fetches_the_next() {
    let server = server().await;
    let player = Player::new(&server);
    // The block's last byte: answered only once the block is complete.
    let last = BLOCK - 1;
    assert_partial(
        &player
            .play(TICKET, &[("range", &format!("bytes={last}-{last}"))])
            .await,
        last,
        1,
        LARGE,
    );
    assert_eq!(
        upstream_ranges(&server),
        vec![
            format!("bytes=0-{}", BLOCK - 1),
            format!("bytes={BLOCK}-{}", 2 * BLOCK - 1),
        ]
    );
    // The player's next read finds it held.
    let next = BLOCK + 5;
    assert_partial(
        &player
            .range(TICKET, &format!("bytes={next}-{}", next + 99))
            .await,
        next,
        100,
        LARGE,
    );
    // Reading on in the first block asks for nothing more.
    player.play(TICKET, &[("range", "bytes=0-99")]).await;
    assert_eq!(server.seen().len(), 2);
}

#[tokio::test]
async fn nothing_is_read_ahead_while_a_block_is_still_arriving() {
    let server = server().await;
    let player = Player::new(&server);
    // "slow" sends 320 KiB of the block and then holds.
    assert_partial(
        &player.play("slow", &[("range", "bytes=0-99")]).await,
        0,
        100,
        LARGE,
    );
    assert_eq!(server.seen().len(), 1);
}

#[tokio::test]
async fn nothing_is_read_ahead_past_the_end() {
    let server = server().await;
    let player = Player::new(&server);
    let last = LARGE - 1;
    assert_partial(
        &player
            .play(TICKET, &[("range", &format!("bytes={last}-{last}"))])
            .await,
        last,
        1,
        LARGE,
    );
    assert_eq!(
        upstream_ranges(&server),
        vec![format!(
            "bytes={}-{}",
            block_start(last as u64),
            3 * BLOCK - 1
        )]
    );
}

#[tokio::test]
async fn the_probe_never_reads_ahead() {
    let server = server().await;
    let player = Player::new(&server);
    // A complete first block, without reading ahead from it.
    let last = BLOCK - 1;
    player.range(TICKET, &format!("bytes={last}-{last}")).await;
    let before = server.seen().len();
    // A read inside it would now fetch the next block; the probe does not.
    player
        .play(
            TICKET,
            &[("range", "bytes=0-0"), ("axon-stream-probe", "1")],
        )
        .await;
    assert_eq!(upstream_ranges(&server)[before..], ["bytes=0-0".to_owned()]);
}

// ===== review round 1 =====

#[tokio::test]
async fn a_ranged_head_keeps_its_content_range() {
    let server = server().await;
    let response = media::respond(
        &transport(),
        &MediaWindows::default(),
        Some(server.base()),
        &scheme_request("HEAD", TICKET, &[("range", "bytes=0-99")]),
        Some(ORIGIN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        header_of(&response, header::CONTENT_RANGE),
        format!("bytes 0-99/{LARGE}")
    );
    assert_eq!(header_of(&response, header::CONTENT_LENGTH), "100");
    assert!(response.body().is_empty());
}

#[tokio::test]
async fn a_block_holds_no_more_than_the_bound_whatever_the_server_sends() {
    let server = server().await;
    let player = Player::new(&server);
    // The block's last byte, so its whole read has run.
    let last = BLOCK - 1;
    assert_partial(
        &player.range("wide", &format!("bytes={last}-{last}")).await,
        last,
        1,
        LARGE,
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(player.windows.held_bytes(), BLOCK);
    // The next block is its own fetch, and what it serves is right.
    let next = BLOCK + 3;
    assert_partial(
        &player
            .range("wide", &format!("bytes={next}-{}", next + 9))
            .await,
        next,
        10,
        LARGE,
    );
    assert_eq!(server.seen().len(), 2);
}

#[tokio::test]
async fn a_span_as_wide_as_u64_is_a_502_on_either_path() {
    let server = server().await;
    let player = Player::new(&server);
    let windowed = player.range("boundless", "bytes=0-99").await;
    assert_eq!(windowed.status(), StatusCode::BAD_GATEWAY);
    let forwarded = player
        .get(
            "boundless",
            &[("range", "bytes=0-99"), ("axon-stream-probe", "1")],
        )
        .await;
    assert_eq!(forwarded.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn dropping_a_block_wakes_the_requests_waiting_on_it() {
    let server = server().await;
    let player = Player::new(&server);
    // "slow" sends 320 KiB of the block and then holds.
    assert_partial(&player.range("slow", "bytes=0-99").await, 0, 100, LARGE);
    let parked = {
        let player = Arc::clone(&player);
        tokio::spawn(async move { player.range("slow", "bytes=1000000-1000099").await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    player.windows.clear();
    let response = tokio::time::timeout(Duration::from_secs(5), parked)
        .await
        .expect("the waiting request was not woken")
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn a_full_window_drops_a_complete_block_before_one_still_arriving() {
    let server = server().await;
    let player = Player::new(&server);
    // Still arriving, and the least recently used.
    assert_partial(&player.range("slow", "bytes=0-99").await, 0, 100, LARGE);
    let last = BLOCK - 1;
    for i in 0..MAX_BLOCKS {
        let ticket = format!("t{i}");
        // The block's last byte, so each is complete.
        player.range(&ticket, &format!("bytes={last}-{last}")).await;
    }
    assert_eq!(player.windows.len(), MAX_BLOCKS);
    let slow_fetches = |server: &Server| {
        server
            .seen()
            .iter()
            .filter(|(path, _)| path.ends_with("/slow"))
            .count()
    };
    assert_eq!(slow_fetches(&server), 1);
    assert_partial(
        &player.range("slow", "bytes=100-199").await,
        100,
        100,
        LARGE,
    );
    assert_eq!(slow_fetches(&server), 1, "the arriving block was dropped");
}

// ===== review round 2 =====

#[tokio::test]
async fn a_window_full_of_waited_on_blocks_sends_the_next_request_upstream() {
    let server = server().await;
    let player = Player::new(&server);
    // Every block held is still arriving, and a request waits on each for
    // bytes "slow" has not sent.
    let mut parked = Vec::new();
    for i in 0..MAX_BLOCKS {
        let ticket = format!("slow{i}");
        assert_partial(&player.range(&ticket, "bytes=0-99").await, 0, 100, LARGE);
        let player = Arc::clone(&player);
        parked.push(tokio::spawn(async move {
            player.range(&ticket, "bytes=1000000-1000099").await
        }));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(player.windows.len(), MAX_BLOCKS);
    // Another object's request is answered, but not from a block of its own.
    assert_partial(&player.range(TICKET, "bytes=0-99").await, 0, 100, LARGE);
    assert_eq!(upstream_ranges(&server).last().unwrap(), "bytes=0-99");
    assert_eq!(player.windows.len(), MAX_BLOCKS);
    // No arriving block was dropped for it: each still serves its own bytes
    // without asking again.
    let before = server.seen().len();
    for i in 0..MAX_BLOCKS {
        let ticket = format!("slow{i}");
        assert_partial(
            &player.range(&ticket, "bytes=100-199").await,
            100,
            100,
            LARGE,
        );
    }
    assert_eq!(server.seen().len(), before);
    // The waiting requests were never failed to make room.
    for task in parked {
        assert!(!task.is_finished());
        task.abort();
    }
}

#[tokio::test]
async fn a_full_window_drops_an_arriving_block_no_request_waits_on() {
    let server = server().await;
    let player = Player::new(&server);
    // Arriving, with nothing waiting on them once each read is answered: as a
    // seek leaves the block it moved away from.
    for i in 0..MAX_BLOCKS {
        let ticket = format!("slow{i}");
        assert_partial(&player.range(&ticket, "bytes=0-99").await, 0, 100, LARGE);
    }
    assert_eq!(player.windows.len(), MAX_BLOCKS);
    // Another object gets a block of its own, in place of the least recently
    // used of them.
    assert_partial(&player.range(TICKET, "bytes=0-99").await, 0, 100, LARGE);
    assert_eq!(
        upstream_ranges(&server).last().unwrap(),
        &format!("bytes=0-{}", BLOCK - 1)
    );
    assert_eq!(player.windows.len(), MAX_BLOCKS);
    let fetches = |ticket: &str| {
        server
            .seen()
            .iter()
            .filter(|(path, _)| path.ends_with(&format!("/{ticket}")))
            .count()
    };
    // The dropped block is fetched anew; the rest still serve from memory.
    player.range("slow0", "bytes=100-199").await;
    assert_eq!(fetches("slow0"), 2);
    player.range("slow7", "bytes=100-199").await;
    assert_eq!(fetches("slow7"), 1);
}

#[tokio::test]
async fn a_closed_range_to_the_blocks_end_returns_early_on_a_slow_link() {
    let server = server().await;
    let player = Player::new(&server);
    // "slow" sends 320 KiB of each block and then holds. Each range reaches
    // or passes its block's end, the last block's being the object's.
    for (first, last) in [(0, BLOCK - 1), (BLOCK, 3 * BLOCK), (2 * BLOCK, LARGE - 1)] {
        let started = Instant::now();
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            player.range("slow", &format!("bytes={first}-{last}")),
        )
        .await
        .expect("waited for the whole block");
        assert_partial(&response, first, 320 << 10, LARGE);
        assert!(started.elapsed() >= media::EARLY_AFTER);
    }
}

#[tokio::test]
async fn a_closed_range_short_of_the_blocks_end_still_waits_for_its_bytes() {
    let server = server().await;
    let player = Player::new(&server);
    // Past the 320 KiB "slow" has sent, and short of the block's end.
    let waiting = tokio::time::timeout(
        media::EARLY_AFTER + Duration::from_secs(1),
        player.range("slow", "bytes=0-399999"),
    )
    .await;
    assert!(waiting.is_err(), "answered before its bytes arrived");
}

#[tokio::test]
async fn a_slow_but_steady_block_fills_however_long_it_takes() {
    let server = server().await;
    // "trickle" takes about 2.4 s for a block, far past this stall, but
    // never stops sending for longer than 150 ms.
    let player = Player::with_windows(
        &server,
        MediaWindows::with_stall(Duration::from_millis(500)),
    );
    let last = BLOCK - 1;
    assert_partial(
        &player
            .range("trickle", &format!("bytes={last}-{last}"))
            .await,
        last,
        1,
        LARGE,
    );
    assert_eq!(server.seen().len(), 1);
}

#[tokio::test]
async fn a_block_that_stops_sending_fails_after_the_stall() {
    let server = server().await;
    let player = Player::with_windows(
        &server,
        MediaWindows::with_stall(Duration::from_millis(300)),
    );
    // "slow" sends 320 KiB of the block and then holds.
    assert_partial(&player.range("slow", "bytes=0-99").await, 0, 100, LARGE);
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        player.range("slow", "bytes=1000000-1000099"),
    )
    .await
    .expect("a stalled block held its request");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

// ===== review round 4 =====

#[tokio::test]
async fn an_if_range_the_block_matches_is_answered_from_it() {
    let server = server().await;
    let player = Player::new(&server);
    assert_partial(&player.range(TICKET, "bytes=0-99").await, 0, 100, LARGE);
    let before = server.seen().len();
    let response = player
        .get(
            TICKET,
            &[("range", "bytes=100-199"), ("if-range", "\"e1\"")],
        )
        .await;
    assert_partial(&response, 100, 100, LARGE);
    assert_eq!(server.seen().len(), before);
}

#[tokio::test]
async fn an_if_range_the_block_does_not_match_goes_upstream_as_sent() {
    let server = server().await;
    let player = Player::new(&server);
    assert_partial(&player.range(TICKET, "bytes=0-99").await, 0, 100, LARGE);
    // A stale tag, the block's own tag made weak, and a date: none is a
    // strong match, so the server is the one to choose `206` or `200`.
    for validator in ["\"stale\"", "W/\"e1\"", "Sat, 21 Nov 2026 12:00:00 GMT"] {
        let before = server.seen().len();
        let response = player
            .get(
                TICKET,
                &[("range", "bytes=100-199"), ("if-range", validator)],
            )
            .await;
        // The test server ignores `If-Range`, so it answers the range.
        assert_partial(&response, 100, 100, LARGE);
        let seen = server.seen();
        assert_eq!(seen.len(), before + 1, "{validator}");
        let (_, headers) = seen.last().unwrap();
        assert_eq!(headers.get(header::RANGE).unwrap(), "bytes=100-199");
        assert_eq!(headers.get(header::IF_RANGE).unwrap(), validator);
    }
}

#[tokio::test]
async fn a_block_unused_for_the_idle_time_is_not_read_again() {
    let server = server().await;
    let player = Player::with_windows(&server, MediaWindows::with_idle(Duration::from_millis(200)));
    // The block's last byte, so it is complete.
    let last = BLOCK - 1;
    assert_partial(
        &player.range(TICKET, &format!("bytes={last}-{last}")).await,
        last,
        1,
        LARGE,
    );
    assert_partial(&player.range(TICKET, "bytes=0-99").await, 0, 100, LARGE);
    assert_eq!(server.seen().len(), 1);
    tokio::time::sleep(Duration::from_millis(400)).await;
    // The same block, not a new one: it is fetched anew all the same.
    assert_partial(&player.range(TICKET, "bytes=0-99").await, 0, 100, LARGE);
    assert_eq!(server.seen().len(), 2);
}
