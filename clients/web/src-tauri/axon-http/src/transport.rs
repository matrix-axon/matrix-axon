//! The request table and its lifecycle (ADR 0127 §§ 2, 6, 7), independent of
//! Tauri so it can be tested against a real server without a webview.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use reqwest::header::{self, HeaderMap};
use reqwest::{Method, StatusCode, Url};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::client::{self, ClientOptions, Counters, Stats};
use crate::error::{classify_body, classify_send, is_reset_failure, Kind, TransportError};

/// How soon after sending a `reset` still reads as the stale-connection race
/// rather than network trouble (ADR 0127 § 7).
const FAST_RESET_WINDOW: Duration = Duration::from_secs(1);

/// How long a cancel for a request that has not been sent yet is remembered.
/// The binding cancels only requests it has sent, so a cancel normally finds
/// its entry; this covers the IPC delivering the cancel first, and bounds the
/// one left behind when a cancel races a request that just finished.
const EARLY_CANCEL_TTL: Duration = Duration::from_secs(30);

/// A request's place in the table: the webview it came from and the id the
/// JavaScript side chose for it.
pub(crate) type Key = (String, u64);

pub(crate) struct Outgoing {
    pub(crate) method: Method,
    pub(crate) url: Url,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Option<Vec<u8>>,
}

/// What `send` resolves with once the response headers arrive.
#[derive(Debug, Serialize)]
pub struct Head {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub url: String,
    /// `h2` or `http/1.1`, recorded on `api` traces (ADR 0127 § 5).
    pub version: &'static str,
    /// `false` when the response is known to be empty and so was never
    /// stored: the caller builds a null-body `Response` and never calls
    /// `read_body` for it.
    pub body: bool,
}

struct Entry {
    /// The client the request went out on. A `reset` replaces the current
    /// client, and this one lives until its last entry leaves.
    _client: reqwest::Client,
    cancel: CancellationToken,
    body: Body,
}

enum Body {
    /// Headers have not arrived. The send task holds the request.
    Pending,
    Ready(reqwest::Response),
    /// A `read_body` holds the response until its chunk arrives.
    Reading,
}

impl Drop for Entry {
    /// Leaving the table by any route stops whatever task still holds the
    /// request or its body. On HTTP/2 that resets only this stream.
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[derive(Default)]
struct Table {
    entries: HashMap<Key, Entry>,
    early_cancels: HashMap<Key, Instant>,
}

/// The two clients a `reset` replaces together.
///
/// `negotiated` offers HTTP/2 and HTTP/1.1 by ALPN, so it cannot know a server
/// speaks HTTP/2 until a handshake finishes, and a burst of requests to a cold
/// pool opens a connection for each (hyper-util keeps the first to negotiate
/// `h2` and discards the rest). `h2` speaks only HTTP/2, which lets hyper-util
/// hold every request in a burst behind the first connection. Origins move to
/// it once they have answered over HTTP/2.
#[derive(Clone)]
struct Clients {
    negotiated: reqwest::Client,
    h2: reqwest::Client,
}

pub struct Transport {
    options: ClientOptions,
    counters: Arc<Counters>,
    current: RwLock<Clients>,
    /// Origins known to speak HTTP/2, as `https://host:port`. A property of
    /// the server rather than of a connection, so it outlives a `reset`.
    h2_origins: Mutex<HashSet<String>>,
    table: Mutex<Table>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Transport {
    pub(crate) fn new(options: ClientOptions) -> reqwest::Result<Self> {
        let counters = Arc::new(Counters::default());
        let clients = Self::build(&options, &counters)?;
        Ok(Self {
            options,
            counters,
            current: RwLock::new(clients),
            h2_origins: Mutex::new(HashSet::new()),
            table: Mutex::new(Table::default()),
        })
    }

    fn build(options: &ClientOptions, counters: &Arc<Counters>) -> reqwest::Result<Clients> {
        Ok(Clients {
            negotiated: client::build(options, counters, false)?,
            h2: client::build(options, counters, true)?,
        })
    }

    fn current(&self) -> Clients {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Send a request and wait for its headers.
    pub(crate) async fn send(&self, key: Key, outgoing: Outgoing) -> Result<Head, TransportError> {
        if !matches!(outgoing.url.scheme(), "http" | "https") {
            return Err(TransportError::new(Kind::Scope));
        }
        let method = outgoing.method.clone();
        let origin =
            (outgoing.url.scheme() == "https").then(|| outgoing.url.origin().ascii_serialization());
        let clients = self.current();
        let via_h2 = origin
            .as_ref()
            .is_some_and(|origin| lock(&self.h2_origins).contains(origin));
        let client = if via_h2 {
            clients.h2
        } else {
            clients.negotiated
        };
        let cancel = CancellationToken::new();
        {
            let mut table = lock(&self.table);
            if table.early_cancels.remove(&key).is_some() {
                return Err(TransportError::new(Kind::Cancelled));
            }
            if table.entries.contains_key(&key) {
                return Err(TransportError::new(Kind::Other));
            }
            table.entries.insert(
                key.clone(),
                Entry {
                    _client: client.clone(),
                    cancel: cancel.clone(),
                    body: Body::Pending,
                },
            );
        }

        let sent = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(TransportError::new(Kind::Cancelled)),
            sent = self.dispatch(&client, outgoing) => sent,
        };

        if let Some(origin) = origin {
            match &sent {
                Ok(response)
                    if !via_h2
                        && response.version() == reqwest::Version::HTTP_2
                        && response.url().origin().ascii_serialization() == origin =>
                {
                    lock(&self.h2_origins).insert(origin);
                }
                // Anything that went wrong once a connection existed sends the
                // next request back to negotiating, in case the server stopped
                // offering HTTP/2: one that ignores the ALPN offer answers the
                // HTTP/2 preface with an HTTP/1.1 error, which can look like a
                // reset. Failures before a connection exists say nothing about
                // the server, and on a network dropping new connections,
                // forgetting would bring back the burst this client avoids.
                Err(err)
                    if via_h2
                        && !matches!(
                            err.kind,
                            Kind::Cancelled | Kind::ConnectTimeout | Kind::Refused | Kind::Dns
                        ) =>
                {
                    lock(&self.h2_origins).remove(&origin);
                }
                _ => {}
            }
        }

        let mut table = lock(&self.table);
        let response = match sent {
            Ok(response) => response,
            Err(err) => {
                table.entries.remove(&key);
                return Err(err);
            }
        };
        let Some(entry) = table.entries.get_mut(&key) else {
            // Cancelled between the response arriving and this lock.
            return Err(TransportError::new(Kind::Cancelled));
        };
        let head = head_of(&response, &method);
        if head.body {
            entry.body = Body::Ready(response);
        } else {
            table.entries.remove(&key);
        }
        Ok(head)
    }

    /// One logical request, with the one retry the send path owns: a `GET` or
    /// `HEAD` reset within [`FAST_RESET_WINDOW`] of being sent. That is a
    /// pooled connection the server closed just as the request left. `hyper`
    /// drops a connection that errored, so the retry goes out on another one.
    async fn dispatch(
        &self,
        client: &reqwest::Client,
        outgoing: Outgoing,
    ) -> Result<reqwest::Response, TransportError> {
        let request = build_request(client, outgoing)?;
        let again = if matches!(*request.method(), Method::GET | Method::HEAD) {
            request.try_clone()
        } else {
            None
        };
        Counters::bump(&self.counters.requests_sent);
        let started = Instant::now();
        let result = match client.execute(request).await {
            Err(err) if started.elapsed() < FAST_RESET_WINDOW && is_reset_failure(&err) => {
                match again {
                    Some(again) => {
                        Counters::bump(&self.counters.fast_reset_retries);
                        client.execute(again).await
                    }
                    None => Err(err),
                }
            }
            result => result,
        };
        result.map_err(|err| classify_send(&err))
    }

    /// The next chunk of a body, or `None` at its end. The entry leaves the
    /// table at the end, on an error, and on a cancel.
    pub(crate) async fn read_body(&self, key: &Key) -> Result<Option<Vec<u8>>, TransportError> {
        let (mut response, cancel) = {
            let mut table = lock(&self.table);
            let Some(entry) = table.entries.get_mut(key) else {
                return Err(TransportError::new(Kind::Cancelled));
            };
            match std::mem::replace(&mut entry.body, Body::Reading) {
                Body::Ready(response) => (response, entry.cancel.clone()),
                // A read while another is outstanding, or before headers:
                // the binding pulls one chunk at a time, after `send`.
                other => {
                    entry.body = other;
                    return Err(TransportError::new(Kind::Other));
                }
            }
        };

        let chunk = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(TransportError::new(Kind::Cancelled)),
            chunk = response.chunk() => chunk,
        };

        let mut table = lock(&self.table);
        match chunk {
            Ok(Some(bytes)) => {
                if let Some(entry) = table.entries.get_mut(key) {
                    entry.body = Body::Ready(response);
                }
                Ok(Some(bytes.to_vec()))
            }
            Ok(None) => {
                table.entries.remove(key);
                Ok(None)
            }
            Err(err) => {
                table.entries.remove(key);
                Err(classify_body(&err))
            }
        }
    }

    /// Abort a request at any stage. A cancel that arrives before its `send`
    /// is remembered, and that `send` fails as cancelled without going out.
    pub(crate) fn cancel(&self, key: Key) {
        let mut table = lock(&self.table);
        if table.entries.remove(&key).is_some() {
            return;
        }
        let now = Instant::now();
        table
            .early_cancels
            .retain(|_, at| now.duration_since(*at) < EARLY_CANCEL_TTL);
        table.early_cancels.insert(key, now);
    }

    /// Release a response whose body the caller will not read.
    pub(crate) fn drop_body(&self, key: &Key) {
        lock(&self.table).entries.remove(key);
    }

    /// Swap in a freshly built client. Requests in flight finish on the one
    /// they were sent on (ADR 0127 § 6).
    pub(crate) fn reset(&self) -> Result<(), TransportError> {
        let clients = Self::build(&self.options, &self.counters)
            .map_err(|_| TransportError::new(Kind::Other))?;
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = clients;
        Counters::bump(&self.counters.resets);
        Ok(())
    }

    pub(crate) fn stats(&self) -> Stats {
        let live = lock(&self.table).entries.len();
        self.counters.snapshot(live)
    }

    /// The webview went away: whatever it still had in flight is cancelled.
    pub(crate) fn forget_webview(&self, label: &str) {
        let mut table = lock(&self.table);
        table.entries.retain(|(owner, _), _| owner != label);
        table.early_cancels.retain(|(owner, _), _| owner != label);
    }
}

#[cfg(test)]
impl Transport {
    fn origin(url: &str) -> String {
        Url::parse(url).unwrap().origin().ascii_serialization()
    }

    pub(crate) fn knows_h2(&self, url: &str) -> bool {
        lock(&self.h2_origins).contains(&Self::origin(url))
    }

    pub(crate) fn learn_h2(&self, url: &str) {
        lock(&self.h2_origins).insert(Self::origin(url));
    }
}

fn build_request(
    client: &reqwest::Client,
    outgoing: Outgoing,
) -> Result<reqwest::Request, TransportError> {
    let Outgoing {
        method,
        url,
        mut headers,
        body,
    } = outgoing;
    // https://fetch.spec.whatwg.org/#http-network-or-cache-fetch step 18,
    // which `tauri-plugin-http` also follows.
    if headers.contains_key(header::RANGE) {
        headers.insert(
            header::ACCEPT_ENCODING,
            header::HeaderValue::from_static("identity"),
        );
    }
    // A `POST` or `PUT` without a body still says `Content-Length: 0`, as
    // `fetch` does. hyper sends no length for an empty body, so it is set here.
    if body.is_none() && matches!(method, Method::POST | Method::PUT) {
        headers.insert(
            header::CONTENT_LENGTH,
            header::HeaderValue::from_static("0"),
        );
    }
    let mut builder = client.request(method, url).headers(headers);
    if let Some(body) = body {
        builder = builder.body(body);
    }
    builder
        .build()
        .map_err(|_| TransportError::new(Kind::Other))
}

fn head_of(response: &reqwest::Response, method: &Method) -> Head {
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let version = match response.version() {
        reqwest::Version::HTTP_2 => "h2",
        reqwest::Version::HTTP_3 => "h3",
        reqwest::Version::HTTP_10 => "http/1.0",
        reqwest::Version::HTTP_09 => "http/0.9",
        _ => "http/1.1",
    };
    Head {
        status: status.as_u16(),
        headers,
        url: response.url().to_string(),
        version,
        body: !known_empty(response, method),
    }
}

/// A response that cannot have a body, so it is never stored: a null-body
/// status, any response to `HEAD`, and `Content-Length: 0`.
fn known_empty(response: &reqwest::Response, method: &Method) -> bool {
    let null_body_status = matches!(
        response.status(),
        StatusCode::SWITCHING_PROTOCOLS
            | StatusCode::EARLY_HINTS
            | StatusCode::NO_CONTENT
            | StatusCode::RESET_CONTENT
            | StatusCode::NOT_MODIFIED
    );
    // The header, not `Response::content_length`: that is the body's size
    // hint, which hyper reports as zero for any response it knows is empty.
    let zero_length = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .is_some_and(|value| value.as_bytes() == b"0");
    null_body_status || zero_length || *method == Method::HEAD
}
