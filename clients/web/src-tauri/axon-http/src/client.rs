//! Building the one `reqwest::Client` (ADR 0127 § 1) and the counters every
//! client the plugin builds shares (§ 5).

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect;
use serde::Serialize;

use crate::error::DnsFailure;

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Below carrier NAT idle timers (some expire a mapping after about 30 s) and
/// below most servers' own idle timeouts. See ADR 0127 § 1.
pub(crate) const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(25);
/// hyper pings once nothing has been *read* for this long while a stream is
/// open, and closes the connection if the ping is not answered within
/// [`H2_KEEP_ALIVE_TIMEOUT`]. "Read" means a response head or body chunk this
/// client consumed. DATA for a stream it has already cancelled is discarded
/// inside `h2` and does not count, so a link still delivering a cancelled
/// body looks silent to hyper. The answer to its ping then queues behind that
/// body. The adaptive window in [`build`] bounds that queue. See ADR 0127's
/// 2026-11-07 amendment (#991).
const H2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(20);
const H2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Counters that outlive any one client, so they keep counting across a
/// `reset` (ADR 0127 § 5).
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) connections_opened: AtomicU64,
    pub(crate) requests_sent: AtomicU64,
    pub(crate) nack_retries: AtomicU64,
    pub(crate) fast_reset_retries: AtomicU64,
    pub(crate) requeues: AtomicU64,
    pub(crate) resets: AtomicU64,
}

impl Counters {
    pub(crate) fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self, live_entries: usize) -> Stats {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        Stats {
            connections_opened: get(&self.connections_opened),
            requests_sent: get(&self.requests_sent),
            retries: Retries {
                nack: get(&self.nack_retries),
                fast_reset: get(&self.fast_reset_retries),
                requeue: get(&self.requeues),
            },
            resets: get(&self.resets),
            live_entries,
        }
    }
}

/// What the `stats` command returns, and what `transport:stats` logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub connections_opened: u64,
    /// Requests the caller sent. A retry is counted under `retries`, not here.
    pub requests_sent: u64,
    pub retries: Retries,
    pub resets: u64,
    pub live_entries: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Retries {
    /// HTTP/2 `REFUSED_STREAM` or graceful `GOAWAY` (ADR 0127 § 7).
    pub nack: u64,
    /// A `GET` or `HEAD` reset within a second of being sent (ADR 0127 § 7).
    pub fast_reset: u64,
    /// A request sent again because the HTTP/2 connection attempt it was
    /// waiting for ended without a connection, before anything was written.
    pub requeue: u64,
}

/// Everything a client is built from. Kept so that `reset` builds an
/// identical one.
#[derive(Clone)]
pub(crate) struct ClientOptions {
    pub(crate) user_agent: String,
    /// Extra trust anchors, so tests can reach an in-process HTTPS server.
    #[cfg(test)]
    pub(crate) extra_roots: Vec<reqwest::Certificate>,
    /// The keep-alive interval and timeout, shortened so a test can watch a
    /// ping go unanswered without waiting 40 s.
    #[cfg(test)]
    pub(crate) keep_alive: (Duration, Duration),
}

impl ClientOptions {
    pub(crate) fn new(user_agent: String) -> Self {
        Self {
            user_agent,
            #[cfg(test)]
            extra_roots: Vec::new(),
            #[cfg(test)]
            keep_alive: (H2_KEEP_ALIVE_INTERVAL, H2_KEEP_ALIVE_TIMEOUT),
        }
    }

    #[cfg(test)]
    fn keep_alive(&self) -> (Duration, Duration) {
        self.keep_alive
    }

    #[cfg(not(test))]
    fn keep_alive(&self) -> (Duration, Duration) {
        (H2_KEEP_ALIVE_INTERVAL, H2_KEEP_ALIVE_TIMEOUT)
    }
}

/// `h2_only` builds the client for origins known to speak HTTP/2: it offers
/// only `h2` by ALPN, so hyper-util opens one connection per origin and holds
/// concurrent requests until it is ready.
pub(crate) fn build(
    options: &ClientOptions,
    counters: &Arc<Counters>,
    h2_only: bool,
) -> reqwest::Result<reqwest::Client> {
    let (keep_alive_interval, keep_alive_timeout) = options.keep_alive();
    let mut builder = reqwest::Client::builder()
        .user_agent(options.user_agent.as_str())
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .http2_keep_alive_interval(keep_alive_interval)
        .http2_keep_alive_timeout(keep_alive_timeout)
        .http2_keep_alive_while_idle(false)
        // Size the HTTP/2 receive windows from measured bandwidth (BDP),
        // starting at the protocol's 64 KiB, instead of hyper's fixed 5 MiB
        // per connection and 2 MiB per stream. The window is how much the
        // server may have in flight ahead of anything it sends next: a
        // response head, or the answer to a keep-alive ping. At 5 MiB a 3G
        // link (~100 KB/s) could be holding close to a minute of data, so a
        // ping went unanswered and hyper closed a connection that was
        // working (#991). The window grows only while throughput grows, so it
        // stays small on a slow link and still opens up on a fast one.
        .http2_adaptive_window(true)
        // The transport follows redirects itself, choosing the client for
        // each hop.
        .redirect(redirect::Policy::none())
        .dns_resolver(Arc::new(Resolver))
        .connector_layer(CountConnections(Arc::clone(counters)))
        // reqwest's default retries HTTP/2 refusals itself, out of sight of
        // the counters. The transport retries them instead (ADR 0127 § 7).
        .retry(reqwest::retry::never());
    if h2_only {
        builder = builder.http2_prior_knowledge();
    }
    #[cfg(test)]
    for root in &options.extra_roots {
        builder = builder.add_root_certificate(root.clone());
    }
    builder.build()
}

/// The system resolver, as `hyper-util`'s default uses it (a blocking
/// `getaddrinfo` on tokio's blocking pool), but failing with a type the error
/// classifier can recognise.
struct Resolver;

impl Resolve for Resolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((name.as_str(), 0))
                .await
                .map_err(DnsFailure)?;
            let addrs: Addrs = Box::new(addrs.collect::<Vec<SocketAddr>>().into_iter());
            Ok(addrs)
        })
    }
}

/// Counts every connection the client finishes opening, through TLS where
/// there is TLS. Inside the connect timeout, so a connect that times out is
/// not counted.
#[derive(Clone)]
struct CountConnections(Arc<Counters>);

impl<S> tower_layer::Layer<S> for CountConnections {
    type Service = Counted<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Counted {
            inner,
            counters: Arc::clone(&self.0),
        }
    }
}

#[derive(Clone)]
struct Counted<S> {
    inner: S,
    counters: Arc<Counters>,
}

impl<S, Req> tower_service::Service<Req> for Counted<S>
where
    S: tower_service::Service<Req>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let connecting = self.inner.call(req);
        let counters = Arc::clone(&self.counters);
        Box::pin(async move {
            let conn = connecting.await?;
            Counters::bump(&counters.connections_opened);
            Ok(conn)
        })
    }
}
