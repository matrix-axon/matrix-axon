//! The transport against in-process servers that count the connections they
//! accept (ADR 0127 § Verification, "Rust tests").

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::body::{Body as HttpBody, Bytes, Frame, Incoming, SizeHint};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use reqwest::header::HeaderMap;
use reqwest::{Method, Url};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Notify};

use crate::client::ClientOptions;
use crate::error::Kind;
use crate::transport::{Head, Key, Outgoing, Transport};

const CERT: &[u8] = include_bytes!("../testdata/localhost.cert.der");
const KEY: &[u8] = include_bytes!("../testdata/localhost.key.der");

// ===== client side =====

/// A transport that also trusts the test certificate.
fn transport() -> Arc<Transport> {
    let mut options = ClientOptions::new("Axon/test (test)".into());
    options.extra_roots = vec![reqwest::Certificate::from_der(CERT).unwrap()];
    Arc::new(Transport::new(options).unwrap())
}

/// A transport configured exactly as the app's.
fn production_transport() -> Arc<Transport> {
    Arc::new(Transport::new(ClientOptions::new("Axon/test (test)".into())).unwrap())
}

fn key(id: u64) -> Key {
    ("main".to_owned(), id)
}

fn request(method: Method, url: &str) -> Outgoing {
    Outgoing {
        method,
        url: Url::parse(url).unwrap(),
        headers: HeaderMap::new(),
        body: None,
    }
}

fn get(url: &str) -> Outgoing {
    request(Method::GET, url)
}

async fn read_all(transport: &Transport, key: &Key) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = transport.read_body(key).await.unwrap() {
        out.extend(chunk);
    }
    out
}

/// Send and read the whole body.
async fn fetch(transport: &Transport, id: u64, outgoing: Outgoing) -> (Head, Vec<u8>) {
    let head = transport.send(key(id), outgoing).await.unwrap();
    let body = if head.body {
        read_all(transport, &key(id)).await
    } else {
        Vec::new()
    };
    (head, body)
}

/// Bring an HTTP/2 origin to its steady state: the first request negotiates
/// (one connection) and teaches the transport the origin speaks HTTP/2, and
/// the second opens the HTTP/2-only client's connection, which every later
/// request shares.
async fn warm(transport: &Transport, server: &Server) {
    for id in [1000, 1001] {
        let (head, _) = fetch(transport, id, get(&server.url("/"))).await;
        assert_eq!(head.version, "h2");
    }
    assert_eq!(server.accepted(), 2);
    assert_eq!(transport.stats().connections_opened, 2);
}

/// Await something a cancel should finish promptly, failing rather than
/// hanging if it does not.
async fn promptly<T>(fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), fut)
        .await
        .expect("still waiting 5 s after the cancel")
}

fn live(transport: &Transport) -> usize {
    transport.stats().live_entries
}

// ===== hyper test server =====

/// A response body that can stop part way: `Gated` sends its first chunk,
/// then its second only when the gate opens; `Stalled` never sends a second.
enum TestBody {
    Full(Option<Bytes>),
    /// No length and no data: chunked encoding with only the last chunk.
    EmptyChunked,
    Gated(Option<Bytes>, Option<oneshot::Receiver<Bytes>>),
}

impl TestBody {
    fn text(text: &'static str) -> Self {
        Self::Full(Some(Bytes::from_static(text.as_bytes())))
    }

    fn empty() -> Self {
        Self::Full(None)
    }
}

impl HttpBody for TestBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match &mut *self {
            Self::Full(bytes) => Poll::Ready(bytes.take().map(|b| Ok(Frame::data(b)))),
            Self::EmptyChunked => Poll::Ready(None),
            Self::Gated(first, gate) => {
                if let Some(first) = first.take() {
                    return Poll::Ready(Some(Ok(Frame::data(first))));
                }
                let Some(rx) = gate else {
                    return Poll::Ready(None);
                };
                match Pin::new(rx).poll(cx) {
                    Poll::Ready(Ok(second)) => {
                        *gate = None;
                        Poll::Ready(Some(Ok(Frame::data(second))))
                    }
                    // The gate was dropped: end the body there.
                    Poll::Ready(Err(_)) => {
                        *gate = None;
                        Poll::Ready(None)
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self, Self::Full(None))
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Self::Full(Some(bytes)) => SizeHint::with_exact(bytes.len() as u64),
            Self::Full(None) => SizeHint::with_exact(0),
            _ => SizeHint::default(),
        }
    }
}

type Reply = Pin<Box<dyn Future<Output = hyper::Response<TestBody>> + Send>>;
type Handler = Arc<dyn Fn(hyper::Request<Incoming>) -> Reply + Send + Sync>;

fn reply(status: u16, body: TestBody) -> hyper::Response<TestBody> {
    let mut response = hyper::Response::new(body);
    *response.status_mut() = hyper::StatusCode::from_u16(status).unwrap();
    response
}

struct Server {
    port: u16,
    tls: bool,
    accepted: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    /// Notified as each request arrives, before the handler runs.
    arrived: Arc<Notify>,
    /// How connections accepted from now on answer an offer of HTTP/2.
    alpn: Arc<AtomicU8>,
}

impl Server {
    /// HTTP/1.1, or over TLS HTTP/2 or HTTP/1.1 as ALPN decides.
    async fn start(tls: bool, handler: Handler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let arrived = Arc::new(Notify::new());
        let alpn = Arc::new(AtomicU8::new(Alpn::Offer as u8));
        let (a, r, n, mode) = (
            accepted.clone(),
            requests.clone(),
            arrived.clone(),
            alpn.clone(),
        );
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                a.fetch_add(1, Ordering::SeqCst);
                let mode = mode.load(Ordering::SeqCst);
                let acceptor = tls.then(|| match mode {
                    m if m == Alpn::Refuse as u8 => tls_acceptor(&[b"http/1.1"]),
                    m if m == Alpn::Ignore as u8 => tls_acceptor(&[]),
                    _ => tls_acceptor(&[b"h2", b"http/1.1"]),
                });
                let h1_only = mode != Alpn::Offer as u8;
                let (handler, r, n) = (handler.clone(), r.clone(), n.clone());
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        r.fetch_add(1, Ordering::SeqCst);
                        n.notify_waiters();
                        let reply = handler(req);
                        async move { Ok::<_, Infallible>(reply.await) }
                    });
                    let mut builder =
                        hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
                    if h1_only {
                        builder = builder.http1_only();
                    }
                    let _ = match acceptor {
                        Some(acceptor) => {
                            let Ok(stream) = acceptor.accept(stream).await else {
                                return;
                            };
                            builder
                                .serve_connection(TokioIo::new(stream), service)
                                .await
                        }
                        None => {
                            builder
                                .serve_connection(TokioIo::new(stream), service)
                                .await
                        }
                    };
                });
            }
        });
        Self {
            port,
            tls,
            accepted,
            requests,
            arrived,
            alpn,
        }
    }

    fn url(&self, path: &str) -> String {
        if self.tls {
            format!("https://localhost:{}{path}", self.port)
        } else {
            format!("http://127.0.0.1:{}{path}", self.port)
        }
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

/// How a TLS test server answers a client's ALPN offer.
#[derive(Clone, Copy)]
#[repr(u8)]
enum Alpn {
    /// `h2` or `http/1.1`, whichever the client prefers.
    Offer,
    /// Only `http/1.1`: rustls refuses an offer of `h2` alone with a
    /// `no_application_protocol` alert.
    Refuse,
    /// No ALPN at all, as many OpenSSL-based servers do: the handshake
    /// succeeds whatever the client offered, and the server speaks HTTP/1.1.
    Ignore,
}

fn handler<F, Fut>(f: F) -> Handler
where
    F: Fn(hyper::Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = hyper::Response<TestBody>> + Send + 'static,
{
    Arc::new(move |req| Box::pin(f(req)))
}

fn ok_handler() -> Handler {
    handler(|_| async { reply(200, TestBody::text("hello")) })
}

fn tls_acceptor(alpn: &[&[u8]]) -> tokio_rustls::TlsAcceptor {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(CERT.to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KEY.to_vec())),
        )
        .unwrap();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

// ===== raw TCP test server, for connections that go away =====

#[derive(Clone, Copy)]
enum Conduct {
    /// Read the request, wait, then close without answering.
    CloseAfter(Duration),
    Answer,
}

struct RawServer {
    port: u16,
    accepted: Arc<AtomicUsize>,
}

impl RawServer {
    /// `conduct[n]` decides what the n-th connection gets; the last entry
    /// repeats.
    async fn start(conduct: Vec<Conduct>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let conduct = conduct[n.min(conduct.len() - 1)];
                tokio::spawn(async move {
                    read_request(&mut stream).await;
                    match conduct {
                        Conduct::CloseAfter(delay) => {
                            tokio::time::sleep(delay).await;
                            drop(stream);
                        }
                        Conduct::Answer => {
                            let _ = stream
                                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                                .await;
                            // Hold the connection open as a keep-alive server would.
                            let _ = stream.read(&mut [0; 1]).await;
                        }
                    }
                });
            }
        });
        Self { port, accepted }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

/// Read a request head and, if it says so, its body.
async fn read_request(stream: &mut TcpStream) {
    let mut buf = Vec::new();
    let mut byte = [0; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await.unwrap_or(0) == 0 {
            return;
        }
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    let _ = stream.read_exact(&mut body).await;
}

// ===== HTTP/2 server that refuses its first stream =====

struct RefusingServer {
    port: u16,
    accepted: Arc<AtomicUsize>,
    streams: Arc<AtomicUsize>,
}

impl RefusingServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let streams = Arc::new(AtomicUsize::new(0));
        let acceptor = tls_acceptor(&[b"h2"]);
        let (a, s) = (accepted.clone(), streams.clone());
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                a.fetch_add(1, Ordering::SeqCst);
                let (acceptor, s) = (acceptor.clone(), s.clone());
                tokio::spawn(async move {
                    let stream = acceptor.accept(stream).await.unwrap();
                    let mut conn = h2::server::handshake(stream).await.unwrap();
                    while let Some(Ok((_req, mut respond))) = conn.accept().await {
                        if s.fetch_add(1, Ordering::SeqCst) == 0 {
                            respond.send_reset(h2::Reason::REFUSED_STREAM);
                        } else {
                            let response = hyper::Response::new(());
                            let mut body = respond.send_response(response, false).unwrap();
                            body.send_data(Bytes::from_static(b"ok"), true).unwrap();
                        }
                    }
                });
            }
        });
        Self {
            port,
            accepted,
            streams,
        }
    }

    fn url(&self) -> String {
        format!("https://localhost:{}/", self.port)
    }
}

// ===== connection reuse =====

#[tokio::test]
async fn http1_requests_share_one_keep_alive_connection() {
    let server = Server::start(false, ok_handler()).await;
    let transport = transport();
    for id in 0..5 {
        let (head, body) = fetch(&transport, id, get(&server.url("/"))).await;
        assert_eq!(head.version, "http/1.1");
        assert_eq!(body, b"hello");
    }
    assert_eq!(server.accepted(), 1);
    let stats = transport.stats();
    assert_eq!(stats.connections_opened, 1);
    assert_eq!(stats.requests_sent, 5);
    assert_eq!(stats.live_entries, 0);
}

/// Ten concurrent requests to a cold pool.
async fn burst(transport: &Arc<Transport>, server: &Server, first_id: u64) {
    let mut tasks = tokio::task::JoinSet::new();
    for id in first_id..first_id + 10 {
        let (transport, url) = (transport.clone(), server.url("/"));
        tasks.spawn(async move { fetch(&transport, id, get(&url)).await });
    }
    while let Some(done) = tasks.join_next().await {
        let (head, body) = done.unwrap();
        assert_eq!(head.version, "h2");
        assert_eq!(body, b"hello");
    }
}

#[tokio::test]
async fn http2_requests_share_one_connection() {
    let server = Server::start(true, ok_handler()).await;
    let transport = transport();
    warm(&transport, &server).await;
    burst(&transport, &server, 0).await;
    assert_eq!(server.accepted(), 2);
    assert_eq!(transport.stats().connections_opened, 2);
    assert_eq!(live(&transport), 0);
}

#[tokio::test]
async fn a_cold_burst_to_a_known_http2_origin_opens_one_connection() {
    let server = Server::start(true, ok_handler()).await;
    let transport = transport();
    fetch(&transport, 100, get(&server.url("/"))).await;
    // What a resume does: every pooled connection goes with the old clients,
    // but the transport still knows this origin speaks HTTP/2.
    transport.reset().unwrap();
    let before = transport.stats().connections_opened;
    burst(&transport, &server, 0).await;
    assert_eq!(transport.stats().connections_opened, before + 1);
    assert_eq!(server.accepted(), 2);
}

#[tokio::test]
async fn a_cold_burst_to_a_new_origin_negotiates_on_every_connection() {
    // Why the HTTP/2-only client exists: until one handshake has negotiated
    // `h2`, hyper-util cannot know to wait for it. This pins the cost the
    // first contact still pays, so a reqwest or hyper-util upgrade that
    // removes it shows up here.
    let server = Server::start(true, ok_handler()).await;
    let transport = transport();
    burst(&transport, &server, 0).await;
    assert!(server.accepted() > 1, "accepted {}", server.accepted());
}

#[tokio::test]
async fn an_origin_that_stops_offering_http2_negotiates_again() {
    for (alpn, kind) in [(Alpn::Refuse, Kind::Tls), (Alpn::Ignore, Kind::Reset)] {
        let server = Server::start(true, ok_handler()).await;
        let transport = transport();
        warm(&transport, &server).await;
        transport.reset().unwrap();
        server.alpn.store(alpn as u8, Ordering::SeqCst);
        // The HTTP/2-only client cannot talk to it...
        let err = transport
            .send(key(0), get(&server.url("/")))
            .await
            .unwrap_err();
        assert_eq!(err.kind, kind);
        // ...and the next request negotiates, and gets HTTP/1.1.
        let (head, body) = fetch(&transport, 1, get(&server.url("/"))).await;
        assert_eq!(head.version, "http/1.1");
        assert_eq!(body, b"hello");
    }
}

#[tokio::test]
async fn a_refused_connect_keeps_the_origin_on_the_http2_client() {
    // Nothing listens here: the failure comes before any connection, so it
    // says nothing about which protocols the server speaks.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!(
            "https://localhost:{}/",
            listener.local_addr().unwrap().port()
        )
    };
    let transport = transport();
    transport.learn_h2(&closed);
    let err = transport.send(key(0), get(&closed)).await.unwrap_err();
    assert_eq!(err.kind, Kind::Refused);
    assert!(transport.knows_h2(&closed));
}

#[tokio::test]
async fn user_agent_and_raw_body_reach_the_server() {
    let server = Server::start(
        false,
        handler(|req: hyper::Request<Incoming>| async move {
            use http_body_util::BodyExt;
            let ua = req.headers()["user-agent"].to_str().unwrap().to_owned();
            let length = req.headers()["content-length"].to_str().unwrap().to_owned();
            let body = req.into_body().collect().await.unwrap().to_bytes();
            let mut echo = format!("{ua}|{length}|").into_bytes();
            echo.extend_from_slice(&body);
            reply(200, TestBody::Full(Some(Bytes::from(echo))))
        }),
    )
    .await;
    let transport = transport();
    let mut post = request(Method::POST, &server.url("/"));
    post.body = Some(b"\x00binary\xff".to_vec());
    let (_, body) = fetch(&transport, 0, post).await;
    assert_eq!(body, b"Axon/test (test)|8|\x00binary\xff".as_slice());
    // A POST without a body still says Content-Length: 0.
    let (_, body) = fetch(&transport, 1, request(Method::POST, &server.url("/"))).await;
    assert_eq!(body, b"Axon/test (test)|0|");
}

#[tokio::test]
async fn a_redirect_from_a_known_http2_origin_to_an_http1_origin_is_followed() {
    let target = Server::start(true, ok_handler()).await;
    target.alpn.store(Alpn::Refuse as u8, Ordering::SeqCst);
    let source = Server::start(true, redirecting(target.url("/"))).await;
    let transport = transport();
    warm(&transport, &source).await;
    // Each hop takes the client its own origin calls for: the HTTP/2-only
    // client for the source, the negotiating one for the target.
    let (head, body) = fetch(&transport, 0, get(&source.url("/away"))).await;
    assert_eq!(head.version, "http/1.1");
    assert_eq!(body, b"hello");
    assert!(transport.knows_h2(&source.url("/")));
    assert!(!transport.knows_h2(&target.url("/")));
}

// ===== cancel and the table =====

#[tokio::test]
async fn cancel_before_headers_releases_the_entry_and_keeps_the_connection() {
    let server = Server::start(
        true,
        handler(|req: hyper::Request<Incoming>| async move {
            if req.uri().path() == "/never" {
                std::future::pending::<()>().await;
            }
            reply(200, TestBody::text("hello"))
        }),
    )
    .await;
    let transport = transport();
    warm(&transport, &server).await;

    let arrived = server.arrived.notified();
    let pending = {
        let (transport, url) = (transport.clone(), server.url("/never"));
        tokio::spawn(async move { transport.send(key(1), get(&url)).await })
    };
    arrived.await;
    assert_eq!(live(&transport), 1);
    transport.cancel(key(1));
    assert_eq!(
        promptly(pending).await.unwrap().unwrap_err().kind,
        Kind::Cancelled
    );
    assert_eq!(live(&transport), 0);

    // On HTTP/2 the cancel reset one stream, not the connection.
    fetch(&transport, 2, get(&server.url("/"))).await;
    assert_eq!(server.accepted(), 2);
    assert_eq!(transport.stats().connections_opened, 2);
}

#[tokio::test]
async fn cancel_mid_body_releases_the_entry() {
    let gates = Arc::new(Mutex::new(Vec::new()));
    let kept = gates.clone();
    let server = Server::start(
        true,
        handler(move |_| {
            let (tx, rx) = oneshot::channel::<Bytes>();
            kept.lock().unwrap().push(tx);
            async move {
                reply(
                    200,
                    TestBody::Gated(Some(Bytes::from_static(b"first")), Some(rx)),
                )
            }
        }),
    )
    .await;
    let transport = transport();
    let head = transport.send(key(0), get(&server.url("/"))).await.unwrap();
    assert!(head.body);
    assert_eq!(
        transport.read_body(&key(0)).await.unwrap().unwrap(),
        b"first"
    );

    // A read waiting on the stalled body is woken by the cancel.
    let reading = {
        let transport = transport.clone();
        tokio::spawn(async move { transport.read_body(&key(0)).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    transport.cancel(key(0));
    assert_eq!(
        promptly(reading).await.unwrap().unwrap_err().kind,
        Kind::Cancelled
    );
    assert_eq!(live(&transport), 0);
    drop(gates);
}

#[tokio::test]
async fn a_cancel_that_arrives_first_stops_the_send() {
    let server = Server::start(false, ok_handler()).await;
    let transport = transport();
    transport.cancel(key(0));
    let err = transport
        .send(key(0), get(&server.url("/")))
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::Cancelled);
    assert_eq!(server.requests(), 0);
    assert_eq!(live(&transport), 0);
    // The early cancel was used up: the id is free again.
    fetch(&transport, 0, get(&server.url("/"))).await;
}

#[tokio::test]
async fn a_cancelled_send_leaves_a_reused_id_alone() {
    let server = Server::start(
        false,
        handler(|_| async {
            std::future::pending::<()>().await;
            reply(200, TestBody::empty())
        }),
    )
    .await;
    let transport = transport();
    let arrived = server.arrived.notified();
    let old = {
        let (transport, url) = (transport.clone(), server.url("/old"));
        tokio::spawn(async move { transport.send(key(1), get(&url)).await })
    };
    arrived.await;
    transport.cancel(key(1));
    // The cancel frees the id at once. Register a new request under it before
    // the cancelled task runs again.
    let mut replacement = Box::pin(transport.send(key(1), get(&server.url("/new"))));
    std::future::poll_fn(|cx| {
        assert!(replacement.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(live(&transport), 1);
    assert_eq!(
        promptly(old).await.unwrap().unwrap_err().kind,
        Kind::Cancelled
    );
    assert_eq!(live(&transport), 1, "the old send removed the new entry");
    transport.cancel(key(1));
    assert_eq!(
        promptly(replacement).await.unwrap_err().kind,
        Kind::Cancelled
    );
}

#[tokio::test]
async fn drop_body_releases_an_unread_response() {
    let server = Server::start(false, ok_handler()).await;
    let transport = transport();
    let head = transport.send(key(0), get(&server.url("/"))).await.unwrap();
    assert!(head.body);
    assert_eq!(live(&transport), 1);
    transport.drop_body(&key(0));
    assert_eq!(live(&transport), 0);
}

#[tokio::test]
async fn forgetting_a_webview_cancels_its_requests() {
    let server = Server::start(
        false,
        handler(|_| async {
            std::future::pending::<()>().await;
            reply(200, TestBody::empty())
        }),
    )
    .await;
    let transport = transport();
    let arrived = server.arrived.notified();
    let pending = {
        let (transport, url) = (transport.clone(), server.url("/"));
        tokio::spawn(async move { transport.send(key(0), get(&url)).await })
    };
    arrived.await;
    transport.forget_webview("main");
    assert_eq!(
        promptly(pending).await.unwrap().unwrap_err().kind,
        Kind::Cancelled
    );
    assert_eq!(live(&transport), 0);
}

#[tokio::test]
async fn empty_bodies_never_linger() {
    let server = Server::start(
        false,
        handler(|req: hyper::Request<Incoming>| async move {
            match req.uri().path() {
                "/204" => reply(204, TestBody::empty()),
                "/zero" => reply(200, TestBody::empty()),
                "/chunked" => reply(200, TestBody::EmptyChunked),
                _ => reply(200, TestBody::text("a body HEAD never sees")),
            }
        }),
    )
    .await;
    let transport = transport();

    let head = transport
        .send(key(0), request(Method::HEAD, &server.url("/")))
        .await
        .unwrap();
    assert!(!head.body);
    assert_eq!(live(&transport), 0);

    let head = transport
        .send(key(1), get(&server.url("/204")))
        .await
        .unwrap();
    assert_eq!(head.status, 204);
    assert!(!head.body);
    assert_eq!(live(&transport), 0);

    let head = transport
        .send(key(2), get(&server.url("/zero")))
        .await
        .unwrap();
    assert!(!head.body);
    assert_eq!(live(&transport), 0);

    // Not known to be empty until the first read finds the end.
    let head = transport
        .send(key(3), get(&server.url("/chunked")))
        .await
        .unwrap();
    assert!(head.body);
    assert_eq!(live(&transport), 1);
    assert_eq!(transport.read_body(&key(3)).await.unwrap(), None);
    assert_eq!(live(&transport), 0);
}

// ===== reset =====

#[tokio::test]
async fn reset_leaves_requests_in_flight_on_the_old_client() {
    let gates = Arc::new(Mutex::new(Vec::new()));
    let kept = gates.clone();
    let server = Server::start(
        true,
        handler(move |_| {
            let (tx, rx) = oneshot::channel::<Bytes>();
            kept.lock().unwrap().push(tx);
            async move {
                reply(
                    200,
                    TestBody::Gated(Some(Bytes::from_static(b"one,")), Some(rx)),
                )
            }
        }),
    )
    .await;
    let transport = transport();
    // The first request negotiates and teaches the transport the origin
    // speaks HTTP/2; the two after it share the HTTP/2-only client's
    // connection.
    let (head, _) = fetch_after_gate(&transport, &gates, 1000, &server).await;
    assert_eq!(head.version, "h2");
    for id in [0, 1] {
        transport
            .send(key(id), get(&server.url("/")))
            .await
            .unwrap();
        assert_eq!(
            transport.read_body(&key(id)).await.unwrap().unwrap(),
            b"one,"
        );
    }
    assert_eq!(transport.stats().connections_opened, 2);

    transport.reset().unwrap();

    // Request 0 can still be read to its end...
    gates
        .lock()
        .unwrap()
        .remove(0)
        .send(Bytes::from_static(b"two"))
        .unwrap();
    assert_eq!(read_all(&transport, &key(0)).await, b"two");
    // ...and request 1 can still be cancelled.
    transport.cancel(key(1));
    assert_eq!(live(&transport), 0);

    // A new request takes the new client, and so a new connection.
    transport.send(key(2), get(&server.url("/"))).await.unwrap();
    transport.drop_body(&key(2));
    let stats = transport.stats();
    assert_eq!(stats.resets, 1);
    assert_eq!(stats.requests_sent, 4);
    assert_eq!(stats.connections_opened, 3);
    assert_eq!(server.accepted(), 3);
    assert_eq!(stats.live_entries, 0);
}

/// A whole request to the gated server, opening its gate as soon as the
/// first chunk is in.
async fn fetch_after_gate(
    transport: &Transport,
    gates: &Mutex<Vec<oneshot::Sender<Bytes>>>,
    id: u64,
    server: &Server,
) -> (Head, Vec<u8>) {
    let head = transport
        .send(key(id), get(&server.url("/")))
        .await
        .unwrap();
    let mut body = transport.read_body(&key(id)).await.unwrap().unwrap();
    gates
        .lock()
        .unwrap()
        .pop()
        .unwrap()
        .send(Bytes::new())
        .unwrap();
    body.extend(read_all(transport, &key(id)).await);
    (head, body)
}

// ===== error categories =====

#[tokio::test]
async fn refused_for_a_closed_port() {
    // A port nothing listens on. Not a bound socket: on macOS a connect to one
    // that never listens times out instead of being refused.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let transport = transport();
    let err = transport
        .send(key(0), get(&format!("http://127.0.0.1:{port}/")))
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::Refused);
    assert_eq!(err.detail.as_deref(), Some("ConnectionRefused"));
    assert_eq!(live(&transport), 0);
}

#[tokio::test]
async fn dns_for_an_invalid_host() {
    let transport = transport();
    let err = transport
        .send(key(0), get("http://axon-test.invalid/"))
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::Dns);
    assert_eq!(err.detail, None);
}

#[tokio::test]
async fn tls_for_a_self_signed_certificate() {
    let server = Server::start(true, ok_handler()).await;
    let err = production_transport()
        .send(key(0), get(&server.url("/")))
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::Tls);
    assert_eq!(err.detail.as_deref(), Some("UnknownIssuer"));
}

#[tokio::test]
async fn reset_for_a_server_that_closes_after_reading() {
    let server = RawServer::start(vec![Conduct::CloseAfter(Duration::ZERO)]).await;
    let transport = transport();
    let err = transport
        .send(key(0), request(Method::POST, &server.url()))
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::Reset);
    assert_eq!(live(&transport), 0);
}

#[tokio::test]
async fn scope_for_other_schemes_and_redirect_hops() {
    let transport = transport();
    let err = transport
        .send(key(0), get("file:///etc/passwd"))
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::Scope);

    let server = Server::start(
        false,
        handler(|_| async {
            let mut response = reply(302, TestBody::empty());
            response
                .headers_mut()
                .insert("location", "ftp://example.com/".parse().unwrap());
            response
        }),
    )
    .await;
    let err = transport
        .send(key(1), get(&server.url("/")))
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::Scope);
    assert_eq!(live(&transport), 0);
}

#[tokio::test]
async fn redirects_stop_after_ten_hops() {
    let server = Server::start(
        false,
        handler(|req: hyper::Request<Incoming>| async move {
            let hop: i32 = req.uri().path()[1..].parse().unwrap();
            if hop == 10 {
                return reply(200, TestBody::text("landed"));
            }
            let mut response = reply(302, TestBody::empty());
            response
                .headers_mut()
                .insert("location", format!("/{}", hop + 1).parse().unwrap());
            response
        }),
    )
    .await;
    let transport = transport();
    // Ten hops are followed...
    let (head, body) = fetch(&transport, 0, get(&server.url("/0"))).await;
    assert_eq!(body, b"landed");
    assert!(head.url.ends_with("/10"));
    // ...and an eleventh is not.
    let err = transport
        .send(key(1), get(&server.url("/-1")))
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::Other);
    assert_eq!(err.detail.as_deref(), Some("TooManyRedirects"));
}

/// Echoes `method|authorization|content-type|body`, after the redirects
/// `/301`, `/303` and `/307` to `/echo`, and `/away` to `away`.
fn redirecting(away: String) -> Handler {
    handler(move |req: hyper::Request<Incoming>| {
        let away = away.clone();
        async move {
            use http_body_util::BodyExt;
            let to = |status, location: &str| {
                let mut response = reply(status, TestBody::empty());
                response
                    .headers_mut()
                    .insert("location", location.parse().unwrap());
                response
            };
            match req.uri().path() {
                "/301" => return to(301, "/echo"),
                "/303" => return to(303, "/echo"),
                "/307" => return to(307, "/echo"),
                "/away" => return to(302, &away),
                _ => {}
            }
            let header = |name| {
                req.headers()
                    .get(name)
                    .map_or("", |value| value.to_str().unwrap())
                    .to_owned()
            };
            let mut echo = format!(
                "{}|{}|{}|",
                req.method(),
                header("authorization"),
                header("content-type")
            )
            .into_bytes();
            echo.extend_from_slice(&req.into_body().collect().await.unwrap().to_bytes());
            reply(200, TestBody::Full(Some(Bytes::from(echo))))
        }
    })
}

#[tokio::test]
async fn redirects_rewrite_methods_and_drop_credentials_as_fetch_does() {
    let other = Server::start(false, redirecting(String::new())).await;
    let server = Server::start(false, redirecting(other.url("/echo"))).await;
    let transport = transport();
    let send = |method: Method, path: &str| {
        let mut outgoing = request(method, &server.url(path));
        outgoing
            .headers
            .insert("authorization", "Bearer t".parse().unwrap());
        outgoing
            .headers
            .insert("content-type", "text/plain".parse().unwrap());
        outgoing.body = Some(b"x".to_vec());
        outgoing
    };
    let cases = [
        (Method::POST, "/303", "GET|Bearer t||"),
        (Method::POST, "/301", "GET|Bearer t||"),
        (Method::PUT, "/301", "PUT|Bearer t|text/plain|x"),
        (Method::POST, "/307", "POST|Bearer t|text/plain|x"),
        // Another port is another origin: the token stays behind.
        (Method::GET, "/away", "GET||text/plain|x"),
    ];
    for (id, (method, path, expected)) in cases.into_iter().enumerate() {
        let (head, body) = fetch(&transport, id as u64, send(method.clone(), path)).await;
        assert_eq!(
            String::from_utf8(body).unwrap(),
            expected,
            "{method} {path}"
        );
        assert!(head.url.ends_with("/echo"), "{}", head.url);
    }
    let stats = transport.stats();
    assert_eq!(stats.requests_sent, 5);
    assert_eq!(stats.live_entries, 0);
}

// ===== retries =====

#[tokio::test]
async fn refused_stream_is_retried_once_for_any_method() {
    let server = RefusingServer::start().await;
    let transport = transport();
    let mut post = request(Method::POST, &server.url());
    post.body = Some(b"send once".to_vec());
    let (head, body) = fetch(&transport, 0, post).await;
    assert_eq!(head.status, 200);
    assert_eq!(head.version, "h2");
    assert_eq!(body, b"ok");
    assert_eq!(server.streams.load(Ordering::SeqCst), 2);
    assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
    let stats = transport.stats();
    assert_eq!(stats.retries.nack, 1);
    assert_eq!(stats.retries.fast_reset, 0);
    assert_eq!(stats.requests_sent, 1);
}

#[tokio::test]
async fn a_fast_get_reset_is_retried_once() {
    let server = RawServer::start(vec![Conduct::CloseAfter(Duration::ZERO), Conduct::Answer]).await;
    let transport = transport();
    let (head, body) = fetch(&transport, 0, get(&server.url())).await;
    assert_eq!(head.status, 200);
    assert_eq!(body, b"ok");
    assert_eq!(server.accepted(), 2);
    assert_eq!(transport.stats().retries.fast_reset, 1);
}

#[tokio::test]
async fn a_fast_get_reset_is_retried_only_once() {
    let server = RawServer::start(vec![Conduct::CloseAfter(Duration::ZERO)]).await;
    let transport = transport();
    let err = transport
        .send(key(0), get(&server.url()))
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::Reset);
    assert_eq!(server.accepted(), 2);
    assert_eq!(transport.stats().retries.fast_reset, 1);
}

#[tokio::test]
async fn a_slow_get_reset_is_not_retried() {
    let server = RawServer::start(vec![
        Conduct::CloseAfter(Duration::from_millis(1300)),
        Conduct::Answer,
    ])
    .await;
    let transport = transport();
    let err = transport
        .send(key(0), get(&server.url()))
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::Reset);
    assert_eq!(server.accepted(), 1);
    assert_eq!(transport.stats().retries.fast_reset, 0);
}

#[tokio::test]
async fn post_and_put_resets_are_never_retried() {
    for method in [Method::POST, Method::PUT] {
        let server =
            RawServer::start(vec![Conduct::CloseAfter(Duration::ZERO), Conduct::Answer]).await;
        let transport = transport();
        let mut outgoing = request(method.clone(), &server.url());
        outgoing.body = Some(b"once".to_vec());
        let err = transport.send(key(0), outgoing).await.unwrap_err();
        assert_eq!(err.kind, Kind::Reset, "{method}");
        assert_eq!(server.accepted(), 1, "{method}");
        let stats = transport.stats();
        assert_eq!(stats.retries.fast_reset, 0, "{method}");
        assert_eq!(stats.retries.nack, 0, "{method}");
    }
}
