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
use crate::transport::Transport;

const ORIGIN: &str = "axon://localhost";
/// Over twice the bound, and not a multiple of it.
const LARGE: usize = 9 * 1024 * 1024 + 123;
const SMALL: usize = 1000;
const TICKET: &str = "AAAAbbbbCCCCddddEEEEffffGGGGhhhhIIIIjjjjKK0";

fn object(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
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
    let Ok((first, last)) = resolve(range, len) else {
        return builder
            .status(416)
            .header(header::CONTENT_RANGE, format!("bytes */{len}"))
            .header(header::CONTENT_LENGTH, 0)
            .body(body(Vec::new(), Some(Ok(()))))
            .unwrap();
    };
    let slice = &whole[first..=last];
    let (sent, end) = match ticket {
        // The server's abort at the withheld tail (ADR 0139 § 3): some bytes,
        // then the body fails.
        "tampered" => (chunks(&slice[..slice.len().min(512 << 10)]), Some(Err(()))),
        // A download still running behind a slow homeserver: 320 KiB, then
        // nothing more for now.
        "slow" => (chunks(&slice[..320 << 10]), None),
        // Ends cleanly but short of its own Content-Length.
        "short" => (chunks(&slice[..slice.len() / 2]), Some(Ok(()))),
        _ => (chunks(slice), Some(Ok(()))),
    };
    builder
        .status(206)
        .header(header::CONTENT_RANGE, format!("bytes {first}-{last}/{len}"))
        .header(header::CONTENT_LENGTH, slice.len())
        .body(body(sent, end))
        .unwrap()
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
    media::respond(&transport(), Some(server.base()), &request, Some(ORIGIN)).await
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
        response.body()[..] == object(total)[first..first + len],
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
async fn the_range_goes_upstream_unchanged_and_nothing_else_does() {
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
    media::respond(&transport(), Some(server.base()), &request, Some(ORIGIN)).await;
    let seen = server.seen();
    assert_eq!(seen.len(), 1);
    let (path, headers) = &seen[0];
    assert_eq!(path, &format!("/axon/v1/media-stream/{TICKET}"));
    assert_eq!(headers.get(header::RANGE).unwrap(), "bytes=0-");
    assert_eq!(headers.get(header::ACCEPT_ENCODING).unwrap(), "identity");
    for absent in ["cookie", "authorization", "origin", "referer", "x-custom"] {
        assert!(headers.get(absent).is_none(), "{absent} was forwarded");
    }
}

#[tokio::test]
async fn an_open_range_is_cut_to_the_bound() {
    let server = server().await;
    let response = get(&server, TICKET, &[("range", "bytes=1000-")]).await;
    assert_partial(&response, 1000, MAX_BODY, LARGE);
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
        let response = get(&server, ticket, &[("range", "bytes=0-2097151")]).await;
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
    let response = media::respond(&transport(), None, &request, Some(ORIGIN)).await;
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
        None,
        &scheme_request("GET", TICKET, &[]),
        Some(ORIGIN),
    )
    .await;
    assert_eq!(no_base.status(), StatusCode::SERVICE_UNAVAILABLE);

    for path in ["", "a/b", "%2e%2e", "..%2Fv1%2Frooms"] {
        let response = media::respond(
            &transport,
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
        Some(base),
        &scheme_request("GET", TICKET, &[("range", "bytes=0-")]),
        Some(ORIGIN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}
