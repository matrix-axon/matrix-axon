//! The loopback HTTP server that streams video and audio in the Linux shell
//! (ADR 0139 § 7, loopback amendment; issue 1192).
//!
//! WebKitGTK's GStreamer player loads only `http`, `https`, `blob`, `data`
//! and `file` URLs, so it cannot play the `axon-media` scheme. Instead the
//! media element loads `http://127.0.0.1:<port>/<token>/<ticket>`, and this
//! server proxies it to `<base>/v1/media-stream/<ticket>` on the shell's
//! pooled client, as the scheme does on the other platforms. Unlike the
//! scheme's responder, a socket can stream, so a response is passed through
//! as it arrives: no 4 MiB bound, no read-ahead window, and a Range-less
//! request (how WebKitGTK opens a media element, issue 1184) gets the
//! server's streamed `200`.
//!
//! Any local process can reach the port, so:
//!
//! - It binds `127.0.0.1` only, on a port the system picks, the first time
//!   the page asks for it ([`Loopback::prefix`]).
//! - The first path segment is a per-launch token of 256 random bits, and a
//!   request without it gets an empty `404`. The ticket after it is a second
//!   credential, which expires and is bound to its account.
//! - `Host` must be exactly `127.0.0.1:<port>`, against DNS rebinding.
//! - A request with an `Origin` other than the page's is refused, and CORS
//!   headers go only to the page's own origin, for the section 6 probe.
//! - Only `GET` and `HEAD` are proxied, only the base's ticket route is
//!   reached, a redirect is a `502` rather than followed, and only the
//!   headers the scheme forwards are passed either way.
//!
//! Tokens and tickets are credentials, so nothing here logs a URL or a path.

use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, PoisonError, RwLock};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use hyper::body::{Body as HttpBody, Bytes, Frame};
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode, Url};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex, Semaphore};

use crate::error::{Kind, TransportError};
use crate::media::{self, ticket_of, upstream_url, DEADLINE, FORWARDED_REQUEST};
use crate::media_window::STALL;
use crate::transport::{Outgoing, Transport};

/// Connections served at once. A media element holds one or two; the rest
/// is headroom, so a local process opening sockets cannot exhaust the shell.
const MAX_CONNECTIONS: usize = 64;

/// How long a connection may take to send a request's headers.
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);

/// Upstream chunks buffered per response before the reader waits for the
/// player, so the player's pace reaches the server.
const CHUNKS_IN_FLIGHT: usize = 4;

/// What a connection needs from the shell for each request: the shared
/// client, and the server base the page last set. The app handle in the
/// shell, a fixed pair in tests.
pub(crate) trait Upstream: Clone + Send + Sync + 'static {
    fn transport(&self) -> Option<&Transport>;
    fn base(&self) -> Option<Url>;
}

impl<R: tauri::Runtime> Upstream for tauri::AppHandle<R> {
    fn transport(&self) -> Option<&Transport> {
        use tauri::Manager;
        self.try_state::<Transport>().map(|state| state.inner())
    }

    fn base(&self) -> Option<Url> {
        use tauri::Manager;
        self.try_state::<media::MediaBase>()
            .and_then(|base| base.get())
    }
}

/// What every connection checks a request against.
pub(crate) struct Gate {
    /// `127.0.0.1:<port>`, the only `Host` accepted.
    host: String,
    /// The per-launch token, in hex.
    token: String,
    /// The page's origin, the only `Origin` accepted and the only one CORS
    /// headers name. Set by each [`Loopback::prefix`] call.
    origin: RwLock<Option<String>>,
}

impl Gate {
    fn origin(&self) -> Option<String> {
        self.origin
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

struct Listening {
    gate: Arc<Gate>,
    port: u16,
}

/// The listener, started on first use.
#[derive(Default)]
pub struct Loopback(Mutex<Option<Listening>>);

impl Loopback {
    /// `http://127.0.0.1:<port>/<token>/`, which a ticket completes into the
    /// URL a media element loads. Binds and starts serving the first time.
    /// `origin` is the asking page's, which the server then answers CORS for.
    pub(crate) async fn prefix<U: Upstream>(
        &self,
        upstream: U,
        origin: Option<String>,
    ) -> Result<String, TransportError> {
        let mut listening = self.0.lock().await;
        if listening.is_none() {
            let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .await
                .map_err(|_| TransportError::new(Kind::Other))?;
            let port = listener
                .local_addr()
                .map_err(|_| TransportError::new(Kind::Other))?
                .port();
            let gate = Arc::new(Gate {
                host: format!("127.0.0.1:{port}"),
                token: new_token()?,
                origin: RwLock::new(None),
            });
            tokio::spawn(serve(listener, Arc::clone(&gate), upstream));
            *listening = Some(Listening { gate, port });
        }
        let Listening { gate, port } = listening.as_ref().expect("bound above");
        *gate.origin.write().unwrap_or_else(PoisonError::into_inner) = origin;
        Ok(format!("http://127.0.0.1:{port}/{}/", gate.token))
    }
}

/// 32 bytes from the system's generator, in hex.
pub(crate) fn new_token() -> Result<String, TransportError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| TransportError::new(Kind::Other))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether `a` and `b` are equal, in time that does not depend on where they
/// first differ.
pub(crate) fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Accept connections for as long as the shell runs.
async fn serve<U: Upstream>(listener: TcpListener, gate: Arc<Gate>, upstream: U) {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok(slot) = Arc::clone(&slots).acquire_owned().await else {
            return;
        };
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            // Out of file descriptors, most likely: let some close.
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let gate = Arc::clone(&gate);
        let upstream = upstream.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let gate = Arc::clone(&gate);
                let upstream = upstream.clone();
                async move { Ok::<_, Infallible>(handle(&upstream, &gate, request).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_TIMEOUT)
                .serve_connection(TokioIo::new(stream), service)
                .await;
            drop(slot);
        });
    }
}

/// A response body: nothing, or upstream chunks as they arrive. An `Err`
/// item makes hyper drop the connection, so a body that fails upstream
/// reaches the player as a failed one, never as a short but clean end.
pub(crate) enum Body {
    Empty,
    Stream(mpsc::Receiver<Result<Bytes, std::io::Error>>),
}

impl HttpBody for Body {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        match self.get_mut() {
            Self::Empty => Poll::Ready(None),
            Self::Stream(rx) => rx
                .poll_recv(cx)
                .map(|next| next.map(|item| item.map(Frame::data))),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            Self::Empty => true,
            Self::Stream(_) => false,
        }
    }
}

fn status(code: StatusCode) -> hyper::Response<Body> {
    media::status(code).map(|_| Body::Empty)
}

/// The answer for one request on the loopback port.
pub(crate) async fn handle<U: Upstream, B>(
    upstream: &U,
    gate: &Gate,
    request: hyper::Request<B>,
) -> hyper::Response<Body> {
    // Not for us: an empty `404`, the same whether the host, the token or
    // the ticket was wrong, so that nothing can be learned from the answer.
    let host_ok = request.uri().authority().is_none()
        && request
            .headers()
            .get(header::HOST)
            .is_some_and(|host| same(host.as_bytes(), gate.host.as_bytes()));
    let ticket = request
        .uri()
        .path()
        .strip_prefix('/')
        .and_then(|path| path.split_at_checked(gate.token.len()))
        .filter(|(token, _)| same(token.as_bytes(), gate.token.as_bytes()))
        .and_then(|(_, rest)| ticket_of(rest));
    let (true, Some(ticket)) = (host_ok, ticket) else {
        return status(StatusCode::NOT_FOUND);
    };

    // A media element sends no `Origin`; the page's own probe sends the
    // page's. Anything else is another origin trying the port.
    let page = gate.origin();
    let cors = match request.headers().get(header::ORIGIN) {
        None => None,
        Some(origin)
            if page
                .as_deref()
                .is_some_and(|page| same(origin.as_bytes(), page.as_bytes())) =>
        {
            page.as_deref()
        }
        Some(_) => return status(StatusCode::FORBIDDEN),
    };

    let mut response = if request.method() == Method::OPTIONS {
        if cors.is_some() {
            media::preflight().map(|_| Body::Empty)
        } else {
            status(StatusCode::FORBIDDEN)
        }
    } else {
        proxy(upstream, &request, ticket).await
    };
    if let Some(origin) = cors.and_then(|origin| HeaderValue::from_str(origin).ok()) {
        media::allow_origin(response.headers_mut(), origin);
    }
    response
}

async fn proxy<U: Upstream, B>(
    upstream: &U,
    request: &hyper::Request<B>,
    ticket: &str,
) -> hyper::Response<Body> {
    let method = request.method().clone();
    if method != Method::GET && method != Method::HEAD {
        return status(StatusCode::METHOD_NOT_ALLOWED);
    }
    let (Some(transport), Some(base)) = (upstream.transport(), upstream.base()) else {
        return status(StatusCode::SERVICE_UNAVAILABLE);
    };
    let Some(url) = upstream_url(&base, ticket) else {
        return status(StatusCode::NOT_FOUND);
    };
    let mut headers = HeaderMap::new();
    for name in FORWARDED_REQUEST {
        for value in request.headers().get_all(&name) {
            headers.append(name.clone(), value.clone());
        }
    }
    // Media is already compressed, and the player needs the server's own
    // `Content-Length` and `Content-Range`.
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    let sent = transport.send_once(Outgoing {
        method: method.clone(),
        url,
        headers,
        body: None,
    });
    match tokio::time::timeout(DEADLINE, sent).await {
        Ok(Ok(response)) => pass(&method, response),
        Ok(Err(_)) => status(StatusCode::BAD_GATEWAY),
        Err(_) => status(StatusCode::GATEWAY_TIMEOUT),
    }
}

/// The upstream response as the player gets it: its status and the headers
/// the scheme passes, with the body streamed through.
fn pass(method: &Method, upstream: reqwest::Response) -> hyper::Response<Body> {
    let code = upstream.status();
    // Never followed, and never handed to the player.
    if code.is_redirection() && code != StatusCode::NOT_MODIFIED {
        return status(StatusCode::BAD_GATEWAY);
    }
    let mut headers = media::forwarded_response_headers(upstream.headers());
    for name in [header::CONTENT_LENGTH, header::CONTENT_RANGE] {
        if let Some(value) = upstream.headers().get(&name) {
            headers.insert(name, value.clone());
        }
    }
    let body = if *method == Method::HEAD || code == StatusCode::NOT_MODIFIED {
        Body::Empty
    } else {
        Body::Stream(relay(upstream))
    };
    let mut response = hyper::Response::new(body);
    *response.status_mut() = code;
    *response.headers_mut() = headers;
    response
}

/// Read `upstream`'s body into a channel the response body drains. Ends with
/// an `Err` if the body fails or stalls for [`STALL`], and stops reading,
/// which ends the upstream request, once the player hangs up. A body that
/// ends cleanly short of its `Content-Length` needs nothing here: hyper
/// drops the connection rather than end a response short.
fn relay(mut upstream: reqwest::Response) -> mpsc::Receiver<Result<Bytes, std::io::Error>> {
    let (tx, rx) = mpsc::channel(CHUNKS_IN_FLIGHT);
    tokio::spawn(async move {
        loop {
            let chunk = tokio::select! {
                biased;
                () = tx.closed() => return,
                chunk = tokio::time::timeout(STALL, upstream.chunk()) => chunk,
            };
            let failure = match chunk {
                Ok(Ok(Some(bytes))) => {
                    if tx.send(Ok(bytes)).await.is_err() {
                        return;
                    }
                    continue;
                }
                Ok(Ok(None)) => return,
                Ok(Err(_)) => "failed",
                Err(_) => "stalled",
            };
            let _ = tx.send(Err(std::io::Error::other(failure))).await;
            return;
        }
    });
    rx
}

#[cfg(test)]
impl Gate {
    /// A gate for `127.0.0.1:<port>` with `token`, answering CORS for
    /// `origin`.
    pub(crate) fn new(port: u16, token: &str, origin: Option<&str>) -> Self {
        Self {
            host: format!("127.0.0.1:{port}"),
            token: token.to_owned(),
            origin: RwLock::new(origin.map(str::to_owned)),
        }
    }
}
