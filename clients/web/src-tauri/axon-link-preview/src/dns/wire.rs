//! The resolver on Linux: one question per query, sent to the nameservers in
//! `/etc/resolv.conf`, which is where glibc's own resolver reads them.
//!
//! Not `getaddrinfo` with `AF_INET`: that is the family hint ADR 0132 will
//! not rely on. Not `res_nquery`: its state struct is a private glibc ABI,
//! and glibc 2.34 moved its symbols between libraries. A systemd-resolved
//! stub at 127.0.0.53, or any other local resolver, applies the host's own
//! DNS policy before the question leaves the machine.
//!
//! Built on every platform, so the tests that prove the query types run
//! wherever the tests do; only Linux uses it at runtime.

#![cfg_attr(
    any(target_os = "macos", target_os = "ios", target_os = "android", windows),
    allow(dead_code)
)]

use super::message::{self, Problem};
use super::{Answer, Dns, RecordType};
use crate::error::{Failure, Result};
use crate::Cancel;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::Instant;

/// glibc's `MAXNS`.
const MAX_SERVERS: usize = 3;
/// How long one server gets before the next is tried.
const PER_SERVER: Duration = Duration::from_millis(1500);

#[derive(Clone, Debug)]
pub(crate) struct WireDns {
    servers: Vec<SocketAddr>,
}

impl WireDns {
    pub(crate) fn new(servers: Vec<SocketAddr>) -> Self {
        WireDns { servers }
    }

    /// The servers `/etc/resolv.conf` names, or glibc's default, the local
    /// host, when it names none or cannot be read.
    pub(crate) fn from_system() -> Self {
        let text = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
        WireDns::new(nameservers(&text))
    }
}

/// `nameserver` lines, as glibc reads them: at most three, port 53.
pub(crate) fn nameservers(resolv_conf: &str) -> Vec<SocketAddr> {
    let mut servers: Vec<SocketAddr> = resolv_conf
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            (words.next()? == "nameserver").then_some(())?;
            server(words.next()?)
        })
        .take(MAX_SERVERS)
        .collect();
    if servers.is_empty() {
        servers.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 53));
    }
    servers
}

fn server(text: &str) -> Option<SocketAddr> {
    if let Ok(ip) = text.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, 53));
    }
    // A scoped link-local server, `fe80::1%eth0`.
    let (address, scope) = text.split_once('%')?;
    let ip: std::net::Ipv6Addr = address.parse().ok()?;
    let scope_id = scope
        .parse::<u32>()
        .ok()
        .or_else(|| interface_index(scope))?;
    Some(SocketAddr::V6(std::net::SocketAddrV6::new(
        ip, 53, 0, scope_id,
    )))
}

#[cfg(unix)]
fn interface_index(name: &str) -> Option<u32> {
    let name = std::ffi::CString::new(name).ok()?;
    // SAFETY: `name` is a valid NUL-terminated string for the call.
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    (index != 0).then_some(index)
}

#[cfg(not(unix))]
fn interface_index(_name: &str) -> Option<u32> {
    None
}

fn random_id() -> u16 {
    let mut bytes = [0u8; 2];
    // The OS generator; a predictable id would make spoofing cheaper.
    getrandom::fill(&mut bytes).map_or(0x5aa5, |()| u16::from_be_bytes(bytes))
}

impl Dns for WireDns {
    async fn query(
        &self,
        name: &str,
        record: RecordType,
        deadline: Instant,
        cancel: &Cancel,
    ) -> Result<Answer> {
        let work = async {
            let mut last = Failure::Unavailable;
            for server in &self.servers {
                let slot = (Instant::now() + PER_SERVER).min(deadline);
                match tokio::time::timeout_at(slot, ask(*server, name, record)).await {
                    Ok(Ok(answer)) => return Ok(answer),
                    Ok(Err(failure)) => last = failure,
                    Err(_) => last = Failure::Expired,
                }
                if Instant::now() >= deadline {
                    return Err(Failure::Expired);
                }
            }
            Err(last)
        };
        tokio::select! {
            result = work => result,
            () = cancel.cancelled() => Err(Failure::Cancelled),
        }
    }
}

/// One server: UDP, then TCP if the answer was truncated.
async fn ask(server: SocketAddr, name: &str, record: RecordType) -> Result<Answer> {
    let id = random_id();
    let request = message::query(id, name, record).ok_or(Failure::PreviewUnavailable)?;
    let bind: SocketAddr = match server {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = UdpSocket::bind(bind)
        .await
        .map_err(|_| Failure::Unavailable)?;
    // Connected, so the kernel drops datagrams from anyone but the server.
    socket
        .connect(server)
        .await
        .map_err(|_| Failure::Unavailable)?;
    socket
        .send(&request)
        .await
        .map_err(|_| Failure::Unavailable)?;
    let mut buffer = vec![0u8; usize::from(message::UDP_PAYLOAD)];
    loop {
        let length = socket
            .recv(&mut buffer)
            .await
            .map_err(|_| Failure::Unavailable)?;
        match message::parse(&buffer[..length], Some(id), name, record) {
            Ok(answer) => return Ok(answer),
            // Someone else's answer, or a stale one: keep listening.
            Err(Problem::Mismatch) => continue,
            Err(Problem::Truncated) => return ask_tcp(server, id, &request, name, record).await,
            Err(Problem::ServerFailure | Problem::Malformed) => return Err(Failure::Unavailable),
        }
    }
}

async fn ask_tcp(
    server: SocketAddr,
    id: u16,
    request: &[u8],
    name: &str,
    record: RecordType,
) -> Result<Answer> {
    let mut stream = TcpStream::connect(server)
        .await
        .map_err(|_| Failure::Unavailable)?;
    let length = u16::try_from(request.len()).map_err(|_| Failure::Unavailable)?;
    let mut framed = length.to_be_bytes().to_vec();
    framed.extend_from_slice(request);
    stream
        .write_all(&framed)
        .await
        .map_err(|_| Failure::Unavailable)?;
    let mut prefix = [0u8; 2];
    stream
        .read_exact(&mut prefix)
        .await
        .map_err(|_| Failure::Unavailable)?;
    // At most 64 KiB, by the framing itself.
    let mut response = vec![0u8; usize::from(u16::from_be_bytes(prefix))];
    stream
        .read_exact(&mut response)
        .await
        .map_err(|_| Failure::Unavailable)?;
    message::parse(&response, Some(id), name, record).map_err(|_| Failure::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nameservers_are_read_as_glibc_reads_them() {
        let text = "# comment\nsearch lan\nnameserver 127.0.0.53\nnameserver ::1\noptions edns0\nnameserver 192.0.2.1\nnameserver 192.0.2.2\n";
        assert_eq!(
            nameservers(text),
            vec![
                "127.0.0.53:53".parse().unwrap(),
                "[::1]:53".parse().unwrap(),
                "192.0.2.1:53".parse().unwrap(),
            ]
        );
        assert_eq!(nameservers(""), vec!["127.0.0.1:53".parse().unwrap()]);
        assert_eq!(
            nameservers("nameserver fe80::1%7\n"),
            vec![SocketAddr::V6(std::net::SocketAddrV6::new(
                "fe80::1".parse().unwrap(),
                53,
                0,
                7
            ))]
        );
    }
}
