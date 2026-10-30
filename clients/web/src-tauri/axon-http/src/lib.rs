//! The shell's HTTP transport (ADR 0127).
//!
//! `tauri-plugin-http` 2.5 builds a new `reqwest::Client` for every request,
//! so every request paid its own DNS lookup and TCP and TLS handshakes, and
//! networks that cap new connections per device stalled the app. This plugin
//! holds one client for every request instead: HTTP/2 to the server where TLS
//! negotiates it, a keep-alive pool for plain-`http` LAN servers.
//!
//! The command surface mirrors the plugin's lifecycle, so aborts and streamed
//! bodies carry over:
//!
//! - [`send`] registers a request under an id the JavaScript side chose and
//!   resolves with its [`Head`] once headers arrive. The request line and
//!   headers travel percent-encoded JSON in the [`REQUEST_HEADER`] invoke
//!   header, and the body as the raw invoke payload rather than a JSON array
//!   of numbers.
//! - [`read_body`] pulls the next chunk, so `ReadableStream` backpressure still
//!   reaches the socket.
//! - [`cancel`] aborts at any stage, and [`drop_body`] releases an unread body.
//! - [`reset`] swaps in a fresh client after resume, and [`stats`] returns the
//!   counters `transport:stats` logs.
//!
//! Failures reject with `{ kind, detail }` and no free text (see [`error`]).
//! Any `http` or `https` URL is reachable: the authority is unchanged from the
//! plugin's (ADR 0127 § 8, narrowing tracked in issue 893).

mod client;
mod error;
mod transport;

use percent_encoding::percent_decode_str;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, Url};
use serde::Deserialize;
use tauri::ipc::{InvokeBody, Request, Response};
use tauri::plugin::{Builder, TauriPlugin};
use tauri::{Manager, RunEvent, Runtime, State, Webview, WindowEvent};

pub use client::{Retries, Stats};
pub use error::{Kind, TransportError};
pub use transport::{Head, Transport};

/// The invoke header that carries a request's line and headers, as
/// `encodeURIComponent(JSON.stringify({ id, method, url, headers }))`.
///
/// A header of its own rather than one invoke header per request header: the
/// IPC adds headers of its own (`Content-Type`, `Tauri-Callback`, and on some
/// platforms the webview's `Origin` and `User-Agent`) that would collide.
pub const REQUEST_HEADER: &str = "axon-http-request";

#[derive(Debug, Deserialize)]
struct RequestLine {
    id: u64,
    method: String,
    url: String,
    #[serde(default)]
    headers: Vec<(String, String)>,
}

type Result<T> = std::result::Result<T, TransportError>;

fn malformed() -> TransportError {
    TransportError::new(Kind::Other)
}

fn request_line(headers: &HeaderMap) -> Result<RequestLine> {
    let encoded = headers
        .get(REQUEST_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(malformed)?;
    let json = percent_decode_str(encoded)
        .decode_utf8()
        .map_err(|_| malformed())?;
    serde_json::from_str(&json).map_err(|_| malformed())
}

/// The body, from either IPC path: a raw payload, or a JSON array of numbers
/// where the platform's IPC cannot carry raw bytes. Empty is no body.
fn request_body(body: &InvokeBody) -> Result<Option<Vec<u8>>> {
    let bytes = match body {
        InvokeBody::Raw(bytes) => bytes.clone(),
        InvokeBody::Json(value @ serde_json::Value::Array(_)) => {
            Vec::<u8>::deserialize(value).map_err(|_| malformed())?
        }
        InvokeBody::Json(_) => Vec::new(),
    };
    Ok((!bytes.is_empty()).then_some(bytes))
}

/// Headers `fetch` forbids a page to set, which `tauri-plugin-http` drops as
/// well. A webview's `Request` already refuses most of them; the rest would
/// fight hyper's own framing.
fn is_forbidden(name: &HeaderName) -> bool {
    matches!(
        *name,
        header::ACCEPT_CHARSET
            | header::ACCEPT_ENCODING
            | header::ACCESS_CONTROL_REQUEST_HEADERS
            | header::ACCESS_CONTROL_REQUEST_METHOD
            | header::CONNECTION
            | header::CONTENT_LENGTH
            | header::COOKIE
            | header::DATE
            | header::DNT
            | header::EXPECT
            | header::HOST
            | header::ORIGIN
            | header::REFERER
            | header::SET_COOKIE
            | header::TE
            | header::TRAILER
            | header::TRANSFER_ENCODING
            | header::UPGRADE
            | header::VIA
    ) || name.as_str().starts_with("proxy-")
        || name.as_str().starts_with("sec-")
}

fn outgoing_headers(raw: Vec<(String, String)>, origin: Option<&str>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in raw {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| malformed())?;
        if is_forbidden(&name) {
            continue;
        }
        let value = HeaderValue::from_str(&value).map_err(|_| malformed())?;
        headers.append(name, value);
    }
    // The page's origin, as `tauri-plugin-http` sends it. `url` serialises a
    // `tauri://` origin as the opaque "null", hence the special case.
    if let Some(origin) = origin.and_then(|origin| HeaderValue::from_str(origin).ok()) {
        headers.insert(header::ORIGIN, origin);
    }
    Ok(headers)
}

fn origin_of<R: Runtime>(webview: &Webview<R>) -> Option<String> {
    let url = webview.url().ok()?;
    Some(if url.scheme() == "tauri" {
        "tauri://localhost".to_owned()
    } else {
        url.origin().ascii_serialization()
    })
}

#[tauri::command]
async fn send<R: Runtime>(
    webview: Webview<R>,
    transport: State<'_, Transport>,
    request: Request<'_>,
) -> Result<Head> {
    let line = request_line(request.headers())?;
    let body = request_body(request.body())?;
    let method = Method::from_bytes(line.method.as_bytes()).map_err(|_| malformed())?;
    let url = Url::parse(&line.url).map_err(|_| malformed())?;
    let headers = outgoing_headers(line.headers, origin_of(&webview).as_deref())?;
    let key = (webview.label().to_owned(), line.id);
    transport
        .send(
            key,
            transport::Outgoing {
                method,
                url,
                headers,
                body,
            },
        )
        .await
}

/// The next chunk with a trailing `0`, or the single byte `1` at the end of
/// the body: `tauri-plugin-http`'s framing, so a chunk can be empty.
#[tauri::command]
async fn read_body<R: Runtime>(
    webview: Webview<R>,
    transport: State<'_, Transport>,
    id: u64,
) -> Result<Response> {
    let key = (webview.label().to_owned(), id);
    Ok(Response::new(match transport.read_body(&key).await? {
        Some(mut chunk) => {
            chunk.push(0);
            chunk
        }
        None => vec![1],
    }))
}

#[tauri::command]
fn cancel<R: Runtime>(webview: Webview<R>, transport: State<'_, Transport>, id: u64) {
    transport.cancel((webview.label().to_owned(), id));
}

#[tauri::command]
fn drop_body<R: Runtime>(webview: Webview<R>, transport: State<'_, Transport>, id: u64) {
    transport.drop_body(&(webview.label().to_owned(), id));
}

#[tauri::command]
fn reset(transport: State<'_, Transport>) -> Result<()> {
    transport.reset()
}

#[tauri::command]
fn stats(transport: State<'_, Transport>) -> Stats {
    transport.stats()
}

/// `Axon/<version> (<os>)`, so server and proxy logs can tell the shell from
/// browsers, and from other versions of itself.
fn user_agent(version: &impl std::fmt::Display) -> String {
    format!("Axon/{version} ({})", std::env::consts::OS)
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("axon-http")
        .invoke_handler(tauri::generate_handler![
            send, read_body, cancel, drop_body, reset, stats
        ])
        .setup(|app, _api| {
            let options = client::ClientOptions::new(user_agent(&app.package_info().version));
            app.manage(Transport::new(options)?);
            Ok(())
        })
        .on_event(|app, event| {
            // The shell's webviews each fill a window of the same label.
            if let RunEvent::WindowEvent {
                label,
                event: WindowEvent::Destroyed,
                ..
            } = event
            {
                if let Some(transport) = app.try_state::<Transport>() {
                    transport.forget_webview(label);
                }
            }
        })
        .build()
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod ipc_tests {
    use super::*;

    #[test]
    fn request_line_is_percent_encoded_json() {
        let mut headers = HeaderMap::new();
        // encodeURIComponent('{"id":7,"method":"PUT","url":"https://h/é","headers":[["x-a","1"]]}')
        headers.insert(
            REQUEST_HEADER,
            HeaderValue::from_static(
                "%7B%22id%22%3A7%2C%22method%22%3A%22PUT%22%2C%22url%22%3A%22https%3A%2F%2Fh%2F%C3%A9%22%2C%22headers%22%3A%5B%5B%22x-a%22%2C%221%22%5D%5D%7D",
            ),
        );
        let line = request_line(&headers).unwrap();
        assert_eq!(line.id, 7);
        assert_eq!(line.method, "PUT");
        assert_eq!(line.url, "https://h/é");
        assert_eq!(line.headers, vec![("x-a".to_owned(), "1".to_owned())]);
        assert_eq!(
            request_line(&HeaderMap::new()).unwrap_err().kind,
            Kind::Other
        );
    }

    #[test]
    fn body_comes_raw_or_as_a_number_array() {
        assert_eq!(
            request_body(&InvokeBody::Raw(vec![1, 2])).unwrap(),
            Some(vec![1, 2])
        );
        assert_eq!(
            request_body(&InvokeBody::Json(serde_json::json!([3, 4]))).unwrap(),
            Some(vec![3, 4])
        );
        assert_eq!(request_body(&InvokeBody::Raw(Vec::new())).unwrap(), None);
        assert_eq!(
            request_body(&InvokeBody::Json(serde_json::json!({}))).unwrap(),
            None
        );
    }

    #[test]
    fn forbidden_headers_are_dropped_and_origin_is_ours() {
        let headers = outgoing_headers(
            vec![
                ("Authorization".into(), "Bearer t".into()),
                ("Host".into(), "evil".into()),
                ("Origin".into(), "https://evil".into()),
                ("Sec-Fetch-Mode".into(), "cors".into()),
            ],
            Some("tauri://localhost"),
        )
        .unwrap();
        assert_eq!(headers.get(header::AUTHORIZATION).unwrap(), "Bearer t");
        assert!(headers.get(header::HOST).is_none());
        assert!(headers.get("sec-fetch-mode").is_none());
        assert_eq!(headers.get(header::ORIGIN).unwrap(), "tauri://localhost");
    }

    #[test]
    fn user_agent_names_the_app_version_and_os() {
        let ua = user_agent(&"0.1.18");
        assert!(ua.starts_with("Axon/0.1.18 ("), "{ua}");
    }
}
