//! The Linux shell's loopback media server (ADR 0139 § 7, loopback
//! amendment) against the same in-process ticket route the scheme's tests
//! use.

use std::sync::Arc;

use reqwest::header::{self, HeaderMap};
use reqwest::{Method, StatusCode, Url};

use crate::media_loopback::{handle, new_token, same, Gate, Loopback, Upstream};
use crate::media_tests::{object, server, transport, Server, LARGE, SMALL};
use crate::transport::Transport;

const ORIGIN: &str = "axon://localhost";
const TICKET: &str = "AAAAbbbbCCCCddddEEEEffffGGGGhhhhIIIIjjjjKK0";

#[derive(Clone)]
struct Fixed(Arc<(Transport, Option<Url>)>);

impl Upstream for Fixed {
    fn transport(&self) -> Option<&Transport> {
        Some(&self.0 .0)
    }

    fn base(&self) -> Option<Url> {
        self.0 .1.clone()
    }
}

fn upstream(server: &Server) -> Fixed {
    Fixed(Arc::new((transport(), Some(server.base()))))
}

/// A started loopback server for `server`'s ticket route, and its prefix.
async fn loopback(server: &Server) -> String {
    Loopback::default()
        .prefix(upstream(server), Some(ORIGIN.to_owned()))
        .await
        .unwrap()
}

/// A plain client, as a media element is: nothing of the shell's own.
fn player() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn fetch(url: &str, headers: &[(&str, &str)]) -> reqwest::Response {
    let mut request = player().get(url);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.unwrap()
}

fn header_of(headers: &HeaderMap, name: header::HeaderName) -> &str {
    headers.get(name).map(|v| v.to_str().unwrap()).unwrap_or("")
}

/// The 64 hex digits of a real token, with none of its entropy: a test
/// fixture that no secret scanner mistakes for a credential.
const TOKEN: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const PORT: u16 = 40000;

/// One request straight to the handler, with `Host` set as a webview sets it.
async fn direct(
    server: &Server,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
) -> hyper::Response<crate::media_loopback::Body> {
    let gate = Gate::new(PORT, TOKEN, Some(ORIGIN));
    let mut request = hyper::Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, format!("127.0.0.1:{PORT}"));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    handle(&upstream(server), &gate, request.body(()).unwrap()).await
}

fn path(ticket: &str) -> String {
    format!("/{TOKEN}/{ticket}")
}

#[tokio::test]
async fn a_range_less_request_streams_the_whole_object() {
    let server = server().await;
    let prefix = loopback(&server).await;
    // How WebKitGTK opens a media element (issue 1184), for an object over
    // twice the scheme's 4 MiB bound.
    let response = fetch(&format!("{prefix}{TICKET}"), &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_of(response.headers(), header::CONTENT_LENGTH),
        LARGE.to_string()
    );
    assert_eq!(
        header_of(response.headers(), header::CONTENT_TYPE),
        "video/mp4"
    );
    assert_eq!(
        header_of(response.headers(), header::CACHE_CONTROL),
        "no-store"
    );
    assert_eq!(response.bytes().await.unwrap(), object(LARGE));
    let seen = server.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, format!("/axon/v1/media-stream/{TICKET}"));
    assert!(seen[0].1.get(header::RANGE).is_none());
}

#[tokio::test]
async fn a_range_goes_upstream_as_sent_and_comes_back_whole() {
    let server = server().await;
    let prefix = loopback(&server).await;
    let response = fetch(
        &format!("{prefix}{TICKET}"),
        &[
            ("range", "bytes=100-"),
            ("cookie", "a=b"),
            ("authorization", "Bearer x"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        header_of(response.headers(), header::CONTENT_RANGE),
        format!("bytes 100-{}/{LARGE}", LARGE - 1)
    );
    assert_eq!(response.bytes().await.unwrap(), object(LARGE)[100..]);
    let (_, sent) = &server.seen()[0];
    assert_eq!(sent.get(header::RANGE).unwrap(), "bytes=100-");
    assert_eq!(sent.get(header::ACCEPT_ENCODING).unwrap(), "identity");
    // Only the ticket is a credential: nothing the player sent of its own.
    assert!(sent.get(header::COOKIE).is_none());
    assert!(sent.get(header::AUTHORIZATION).is_none());
    assert!(sent.get(header::ORIGIN).is_none());
}

#[tokio::test]
async fn connections_are_kept_alive_across_requests() {
    let server = server().await;
    let prefix = loopback(&server).await;
    let client = player();
    for first in [0, 5000, 10_000] {
        let response = client
            .get(format!("{prefix}small"))
            .header("range", format!("bytes={first}-{}", first + 99))
            .send()
            .await
            .unwrap();
        // `small` is 1000 bytes: past it, the server's own `416`.
        if first < SMALL {
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(
                response.bytes().await.unwrap(),
                object(SMALL)[first..first + 100]
            );
        } else {
            assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
            assert_eq!(
                header_of(response.headers(), header::CONTENT_RANGE),
                format!("bytes */{SMALL}")
            );
        }
    }
}

#[tokio::test]
async fn anything_but_the_token_host_and_a_ticket_is_an_empty_404() {
    let server = server().await;
    let wrong_token = format!("/{}/{TICKET}", TOKEN.replace('0', "1"));
    let short_token = format!("/{}/{TICKET}", &TOKEN[..63]);
    for uri in [
        wrong_token.as_str(),
        short_token.as_str(),
        &format!("/{TICKET}"),
        &format!("/{TOKEN}"),
        &format!("/{TOKEN}/"),
        &format!("/{TOKEN}/a/b"),
        &format!("/{TOKEN}/a%2Fb"),
        &format!("/{TOKEN}/../{TICKET}"),
        // Absolute-form, as to a proxy.
        &format!("http://127.0.0.1:{PORT}{}", path(TICKET)),
    ] {
        let response = direct(&server, Method::GET, uri, &[]).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
    }
    // DNS rebinding: a name that resolves to the loopback address.
    for host in [
        "localhost:40000",
        "127.0.0.1",
        "evil.example:40000",
        "127.0.0.1:40001",
    ] {
        let gate = Gate::new(PORT, TOKEN, Some(ORIGIN));
        let request = hyper::Request::builder()
            .uri(path(TICKET))
            .header(header::HOST, host)
            .body(())
            .unwrap();
        let response = handle(&upstream(&server), &gate, request).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{host}");
    }
    assert!(server.seen().is_empty(), "a refused request went upstream");
}

#[tokio::test]
async fn another_origin_is_refused_and_only_the_page_gets_cors() {
    let server = server().await;
    let refused = direct(
        &server,
        Method::GET,
        &path(TICKET),
        &[("origin", "https://evil.example")],
    )
    .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert!(refused
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        .is_none());
    let null = direct(&server, Method::GET, &path(TICKET), &[("origin", "null")]).await;
    assert_eq!(null.status(), StatusCode::FORBIDDEN);
    assert!(server.seen().is_empty());

    // The page's probe reads the status and `Content-Range`.
    let probe = direct(
        &server,
        Method::GET,
        &path(TICKET),
        &[
            ("origin", ORIGIN),
            ("range", "bytes=0-0"),
            ("axon-stream-probe", "1"),
        ],
    )
    .await;
    assert_eq!(probe.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        header_of(probe.headers(), header::ACCESS_CONTROL_ALLOW_ORIGIN),
        ORIGIN
    );
    assert!(
        header_of(probe.headers(), header::ACCESS_CONTROL_EXPOSE_HEADERS).contains("Content-Range")
    );
    // The probe header is the scheme's, and goes no further.
    assert!(server.seen()[0].1.get("axon-stream-probe").is_none());

    // A media element sends no `Origin`, and needs no CORS.
    let element = direct(
        &server,
        Method::GET,
        &path(TICKET),
        &[("range", "bytes=0-0")],
    )
    .await;
    assert_eq!(element.status(), StatusCode::PARTIAL_CONTENT);
    assert!(element
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        .is_none());
}

#[tokio::test]
async fn the_probe_preflight_is_answered_for_the_page_alone() {
    let server = server().await;
    let preflight = direct(
        &server,
        Method::OPTIONS,
        &path(TICKET),
        &[
            ("origin", ORIGIN),
            ("access-control-request-method", "GET"),
            ("access-control-request-headers", "range, axon-stream-probe"),
        ],
    )
    .await;
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        header_of(preflight.headers(), header::ACCESS_CONTROL_ALLOW_ORIGIN),
        ORIGIN
    );
    assert!(
        header_of(preflight.headers(), header::ACCESS_CONTROL_ALLOW_HEADERS)
            .contains("Axon-Stream-Probe")
    );
    let bare = direct(&server, Method::OPTIONS, &path(TICKET), &[]).await;
    assert_eq!(bare.status(), StatusCode::FORBIDDEN);
    assert!(server.seen().is_empty());
}

#[tokio::test]
async fn only_get_and_head_are_proxied() {
    let server = server().await;
    for method in [Method::POST, Method::PUT, Method::DELETE] {
        let response = direct(&server, method.clone(), &path(TICKET), &[]).await;
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method}"
        );
    }
    assert!(server.seen().is_empty());

    let prefix = loopback(&server).await;
    let head = player()
        .head(format!("{prefix}{TICKET}"))
        .header("range", "bytes=0-99")
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        header_of(head.headers(), header::CONTENT_RANGE),
        format!("bytes 0-99/{LARGE}")
    );
    assert_eq!(header_of(head.headers(), header::CONTENT_LENGTH), "100");
    assert!(head.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn the_servers_own_errors_and_304_pass_through() {
    let server = server().await;
    let prefix = loopback(&server).await;
    let gone = fetch(&format!("{prefix}gone"), &[("range", "bytes=0-0")]).await;
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        gone.bytes().await.unwrap().as_ref(),
        br#"{"error":"not_found"}"#
    );

    let cached = fetch(&format!("{prefix}{TICKET}"), &[("if-none-match", "\"e1\"")]).await;
    assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn a_redirect_is_a_502_never_followed_or_passed_on() {
    let server = server().await;
    let prefix = loopback(&server).await;
    let response = fetch(&format!("{prefix}moved"), &[("range", "bytes=0-")]).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(response.headers().get(header::LOCATION).is_none());
    assert_eq!(server.seen().len(), 1);
}

#[tokio::test]
async fn a_body_that_fails_or_ends_short_upstream_fails_for_the_player() {
    let server = server().await;
    let prefix = loopback(&server).await;
    // The server's abort at the withheld tail (ADR 0139 § 3), and a body
    // that ends cleanly but short of its `Content-Length`: the player must
    // see a failed read, never a short but clean end.
    for ticket in ["tampered", "short"] {
        let response = fetch(&format!("{prefix}{ticket}"), &[("range", "bytes=0-")]).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT, "{ticket}");
        assert!(response.bytes().await.is_err(), "{ticket} read cleanly");
    }
}

#[tokio::test]
async fn nothing_goes_upstream_until_the_page_sets_a_base() {
    let server = server().await;
    let gate = Gate::new(PORT, TOKEN, Some(ORIGIN));
    let unset = Fixed(Arc::new((transport(), None)));
    let request = hyper::Request::builder()
        .uri(path(TICKET))
        .header(header::HOST, format!("127.0.0.1:{PORT}"))
        .body(())
        .unwrap();
    let response = handle(&unset, &gate, request).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(server.seen().is_empty());
}

#[tokio::test]
async fn one_listener_and_one_token_per_launch() {
    let server = server().await;
    let loopback = Loopback::default();
    let first = loopback
        .prefix(upstream(&server), Some(ORIGIN.to_owned()))
        .await
        .unwrap();
    let again = loopback
        .prefix(upstream(&server), Some(ORIGIN.to_owned()))
        .await
        .unwrap();
    assert_eq!(first, again);
    let url = Url::parse(&first).unwrap();
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_ne!(url.port(), None);
    let token = url.path().trim_matches('/');
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_ne!(new_token().unwrap(), new_token().unwrap());
    assert!(same(b"abc", b"abc"));
    assert!(!same(b"abc", b"abd"));
    assert!(!same(b"abc", b"ab"));
}
