//! The restricted transport, ported from `PreviewNetwork.fetch` and
//! `exchange` (ADR 0132 § 3, "Connection" to "Redirects").
//!
//! - Every hop is validated as a new request: the URL, then DNS, then every
//!   address it returned. At most six requests, redirects followed by hand.
//! - The socket connects to a validated address and nothing else, so a second
//!   resolution cannot rebind it. No proxy is consulted.
//! - TLS 1.2 or later, verified against the original host name with Mozilla's
//!   bundled roots, so verification never makes a network request.
//! - A plain `GET`, with no cookies, credentials or auth headers, and
//!   `Accept-Encoding: identity`.

use crate::address::Address;
use crate::dns::{self, Dns};
use crate::error::{Failure, Result};
use crate::http::{self, Kind, Parser};
use crate::url_check::{self, Checked};
use crate::Cancel;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;
use url::Url;

/// At most this many requests per lookup, the first included.
const HOPS: usize = 6;
/// A `Location` longer than this is refused.
const LOCATION_BYTES: usize = 4096;
/// How long DNS may take on one hop, at most.
const DNS_BUDGET: Duration = Duration::from_secs(4);
/// How long the preferred address gets before the other is tried as well.
pub(crate) const RACE_DELAY: Duration = Duration::from_millis(250);
const USER_AGENT: &str = "AxonLinkPreview/1.0";

#[derive(Clone, Debug)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) body: Vec<u8>,
    pub(crate) url: Url,
}

impl Response {
    pub(crate) fn content_type(&self) -> Option<String> {
        self.headers
            .get("content-type")
            .map(|value| value.to_ascii_lowercase())
    }
}

/// The two steps of a hop, separable so tests can stand in for either, as the
/// Swift tests pass `resolve` and `exchange` closures.
pub(crate) trait Hop: Send + Sync {
    fn resolve(
        &self,
        host: &str,
        deadline: Instant,
        cancel: &Cancel,
    ) -> impl Future<Output = Result<Vec<Address>>> + Send;

    fn exchange(
        &self,
        checked: &Checked,
        addresses: &[Address],
        kind: Kind,
        deadline: Instant,
    ) -> impl Future<Output = Result<Response>> + Send;
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let now = Instant::now();
    if deadline <= now {
        return Err(Failure::Expired);
    }
    Ok(deadline - now)
}

/// `PreviewNetwork.fetch`: one resource, following up to five redirects.
pub(crate) async fn fetch<H: Hop>(
    hop: &H,
    initial: &str,
    kind: Kind,
    deadline: Instant,
    cancel: &Cancel,
) -> Result<Response> {
    crate::log::event(&format!(
        "device_preview_fetch_start kind={}",
        kind.as_str()
    ));
    let mut url = initial.to_owned();
    for index in 0..HOPS {
        if cancel.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        let checked = url_check::validate(&url)?;
        crate::log::event(&format!(
            "device_preview_dns_start kind={} hop={index}",
            kind.as_str()
        ));
        let budget = remaining(deadline)?.min(DNS_BUDGET);
        let addresses = hop
            .resolve(&checked.host, Instant::now() + budget, cancel)
            .await
            .inspect_err(|failure| {
                crate::log::event(&format!("device_preview_dns_failed category={failure:?}"));
            })?;
        if !(1..=2).contains(&addresses.len())
            || !addresses.iter().all(Address::approved_for_connection)
        {
            return Err(Failure::PreviewUnavailable);
        }
        if cancel.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        remaining(deadline)?;
        let response = tokio::select! {
            response = hop.exchange(&checked, &addresses, kind, deadline) => response?,
            () = cancel.cancelled() => return Err(Failure::Cancelled),
        };
        crate::log::event(&format!(
            "device_preview_response kind={} hop={index} status={} bytes={}",
            kind.as_str(),
            response.status,
            response.body.len()
        ));
        if http::is_redirect(response.status) {
            let location = response
                .headers
                .get("location")
                .filter(|location| location.len() <= LOCATION_BYTES)
                .ok_or(Failure::InvalidResponse)?;
            let next = checked
                .url
                .join(location)
                .map_err(|_| Failure::InvalidResponse)?;
            url_check::validate(next.as_str())?;
            url = next.into();
            continue;
        }
        if response.status != 200 {
            return Err(Failure::PreviewUnavailable);
        }
        return Ok(response);
    }
    Err(Failure::PreviewUnavailable)
}

/// `PreviewConnectionRace`: start the first candidate; start the second after
/// `delay`, or as soon as the first fails; the first to succeed wins and the
/// other is dropped, which closes it.
pub(crate) async fn race<C, T, F, Fut>(candidates: &[C], delay: Duration, start: F) -> Result<T>
where
    C: Copy,
    F: Fn(C) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let (first, second) = match candidates {
        [only] => return start(*only).await,
        [first, second] => (*first, *second),
        _ => return Err(Failure::PreviewUnavailable),
    };
    let mut first = pin!(start(first));
    tokio::select! {
        result = &mut first => return match result {
            Ok(connection) => Ok(connection),
            Err(_) => start(second).await,
        },
        () = tokio::time::sleep(delay) => {}
    }
    let mut second = pin!(start(second));
    let mut first_failed = false;
    let mut second_failed = false;
    loop {
        tokio::select! {
            result = &mut first, if !first_failed => match result {
                Ok(connection) => return Ok(connection),
                Err(failure) if second_failed => return Err(failure),
                Err(_) => first_failed = true,
            },
            result = &mut second, if !second_failed => match result {
                Ok(connection) => return Ok(connection),
                Err(failure) if first_failed => return Err(failure),
                Err(_) => second_failed = true,
            },
        }
    }
}

/// Opens the TCP connection to a validated address.
pub(crate) trait Dial: Send + Sync {
    fn dial(&self, address: SocketAddr) -> impl Future<Output = std::io::Result<TcpStream>> + Send;
}

pub(crate) struct Direct;

impl Dial for Direct {
    async fn dial(&self, address: SocketAddr) -> std::io::Result<TcpStream> {
        let stream = TcpStream::connect(address).await?;
        stream.set_nodelay(true)?;
        Ok(stream)
    }
}

/// TLS 1.2 or 1.3, `http/1.1` only, Mozilla's roots, and no session
/// resumption, so one lookup leaves nothing behind for the next.
pub(crate) fn tls_config(roots: RootCertStore) -> Arc<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .expect("ring supports TLS 1.2 and 1.3")
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config.resumption = rustls::client::Resumption::disabled();
    Arc::new(config)
}

pub(crate) fn mozilla_roots() -> RootCertStore {
    RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    }
}

/// The production [`Hop`]: the platform's typed DNS queries, a direct socket,
/// and rustls.
pub(crate) struct Network<D: Dns, C: Dial> {
    pub(crate) dns: D,
    pub(crate) dial: C,
    pub(crate) tls: Arc<ClientConfig>,
    pub(crate) race_delay: Duration,
}

impl<D: Dns, C: Dial> Hop for Network<D, C> {
    async fn resolve(
        &self,
        host: &str,
        deadline: Instant,
        cancel: &Cancel,
    ) -> Result<Vec<Address>> {
        dns::resolve(&self.dns, host, deadline, cancel).await
    }

    async fn exchange(
        &self,
        checked: &Checked,
        addresses: &[Address],
        kind: Kind,
        deadline: Instant,
    ) -> Result<Response> {
        let work = async {
            // Revalidated here too, so no caller can hand this an address or
            // URL the fetch loop did not check.
            if url_check::validate(checked.url.as_str())?.host != checked.host
                || !(1..=2).contains(&addresses.len())
                || !addresses.iter().all(Address::approved_for_connection)
            {
                return Err(Failure::PreviewUnavailable);
            }
            let port = url_check::port(&checked.url);
            let name = ServerName::try_from(checked.host.clone())
                .map_err(|_| Failure::PreviewUnavailable)?;
            let connector = TlsConnector::from(self.tls.clone());
            let stream = race(addresses, self.race_delay, |address: Address| {
                let connector = connector.clone();
                let name = name.clone();
                async move {
                    let family = if address.is_ipv4() { "ipv4" } else { "ipv6" };
                    crate::log::event(&format!("device_preview_connect_start family={family}"));
                    let tcp = self
                        .dial
                        .dial(SocketAddr::new(address.ip(), port))
                        .await
                        .map_err(|_| Failure::Unavailable)?;
                    let tls = connector
                        .connect(name, tcp)
                        .await
                        .map_err(|_| Failure::Unavailable)?;
                    crate::log::event(&format!("device_preview_connect_ready family={family}"));
                    Ok(tls)
                }
            })
            .await?;
            let request = format!(
                "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {USER_AGENT}\r\nAccept: text/html,application/xhtml+xml,image/*;q=0.8\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
                url_check::target(&checked.url),
                url_check::authority(&checked.url, &checked.host),
            );
            read_response(stream, request.as_bytes(), kind, checked.url.clone()).await
        };
        match tokio::time::timeout_at(deadline, work).await {
            Ok(result) => result,
            Err(_) => Err(Failure::Expired),
        }
    }
}

/// Send `request` and collect the response with [`Parser`].
pub(crate) async fn read_response<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    request: &[u8],
    kind: Kind,
    url: Url,
) -> Result<Response> {
    stream
        .write_all(request)
        .await
        .map_err(|_| Failure::Unavailable)?;
    stream.flush().await.map_err(|_| Failure::Unavailable)?;
    let mut parser = Parser::new(kind);
    let mut buffer = vec![0u8; http::READ_BYTES];
    loop {
        let read = match stream.read(&mut buffer).await {
            Ok(read) => read,
            // A server that closes without `close_notify` ends the body the
            // way a plain close does. A truncated image then fails to decode,
            // and a truncated head only loses metadata.
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => 0,
            Err(_) => return Err(Failure::Unavailable),
        };
        let eof = read == 0;
        if parser.append(&buffer[..read], eof)? {
            let (status, headers, body) = parser.into_parts();
            return Ok(Response {
                status,
                headers,
                body,
                url,
            });
        }
        if eof {
            crate::log::event(&format!(
                "device_preview_http_ended headers_received={}",
                parser.status() != 0
            ));
            return Err(Failure::InvalidResponse);
        }
    }
}
