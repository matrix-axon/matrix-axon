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

use crate::error::{is_protocol_nack, DnsFailure, ScopeViolation, TooManyRedirects};

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Below carrier NAT idle timers (some expire a mapping after about 30 s) and
/// below most servers' own idle timeouts. See ADR 0127 § 1.
pub(crate) const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(25);
const H2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(20);
const H2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REDIRECTS: usize = 10;

/// Counters that outlive any one client, so they keep counting across a
/// `reset` (ADR 0127 § 5).
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) connections_opened: AtomicU64,
    pub(crate) requests_sent: AtomicU64,
    pub(crate) nack_retries: AtomicU64,
    pub(crate) fast_reset_retries: AtomicU64,
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
}

/// Everything a client is built from. Kept so that `reset` builds an
/// identical one.
#[derive(Clone)]
pub(crate) struct ClientOptions {
    pub(crate) user_agent: String,
    /// Extra trust anchors, so tests can reach an in-process HTTPS server.
    #[cfg(test)]
    pub(crate) extra_roots: Vec<reqwest::Certificate>,
}

impl ClientOptions {
    pub(crate) fn new(user_agent: String) -> Self {
        Self {
            user_agent,
            #[cfg(test)]
            extra_roots: Vec::new(),
        }
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
    let nack_counters = Arc::clone(counters);
    // A policy built through the public builder replaces reqwest's default
    // (which is private) rather than extending it, and always carries tower's
    // `TpsBudget`: 20% extra over 10 s, with a floor of 10 retries a second.
    let retry = reqwest::retry::for_host(AnyHost)
        .max_retries_per_request(1)
        .classify_fn(move |req_rep| {
            if req_rep.error().is_some_and(is_protocol_nack) {
                // Counted when the policy asks for the retry. The budget can
                // still refuse it, but not below its floor of 10 a second.
                Counters::bump(&nack_counters.nack_retries);
                req_rep.retryable()
            } else {
                req_rep.success()
            }
        });

    let mut builder = reqwest::Client::builder()
        .user_agent(options.user_agent.as_str())
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .http2_keep_alive_interval(H2_KEEP_ALIVE_INTERVAL)
        .http2_keep_alive_timeout(H2_KEEP_ALIVE_TIMEOUT)
        .http2_keep_alive_while_idle(false)
        .redirect(redirect::Policy::custom(check_redirect))
        .dns_resolver(Arc::new(Resolver))
        .connector_layer(CountConnections(Arc::clone(counters)))
        .retry(retry);
    if h2_only {
        builder = builder.http2_prior_knowledge();
    }
    #[cfg(test)]
    for root in &options.extra_roots {
        builder = builder.add_root_certificate(root.clone());
    }
    builder.build()
}

/// Every hop must stay `http` or `https`, and there are at most ten of them.
fn check_redirect(attempt: redirect::Attempt<'_>) -> redirect::Action {
    if !matches!(attempt.url().scheme(), "http" | "https") {
        attempt.error(ScopeViolation)
    } else if attempt.previous().len() > MAX_REDIRECTS {
        attempt.error(TooManyRedirects)
    } else {
        attempt.follow()
    }
}

/// The retry policy's scope, which reqwest only lets you set per host.
struct AnyHost;

impl PartialEq<&str> for AnyHost {
    fn eq(&self, _: &&str) -> bool {
        true
    }
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
