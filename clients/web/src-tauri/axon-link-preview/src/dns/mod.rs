//! Name resolution for previews (ADR 0132 § 3, "DNS").
//!
//! The rule: ask for the origin's **A** records only, and for the **AAAA**
//! records of `ipv4only.arpa` only, never an AAAA query for the origin. A
//! global IPv6 address can be an operator's NAT64 mapping of a private IPv4
//! service, so the only IPv6 address a preview connects to is one synthesized
//! here, from a public A record and an RFC 7050 prefix ([`resolve`]).
//!
//! The rule is kept by asking every platform for a record *type*, never for
//! "addresses" with a family hint that the platform may or may not honour:
//!
//! | Platform        | Query                                                      | Why this one                                                    |
//! | --------------- | ---------------------------------------------------------- | --------------------------------------------------------------- |
//! | macOS, iOS      | `DNSServiceGetAddrInfo`, `kDNSServiceProtocol_IPv4` / `_IPv6` | What the Swift reader uses; mDNSResponder applies VPN, scoped and encrypted DNS |
//! | Android 10+     | `android_res_nquery`, `ns_t_a` / `ns_t_aaaa`                | netd applies Private DNS and the per-network resolver           |
//! | Windows         | `DnsQuery_W`, `DNS_TYPE_A` / `DNS_TYPE_AAAA`                | The DNS Client service applies DoH and the NRPT                 |
//! | Linux and other | One question per query, sent to `/etc/resolv.conf`'s servers ([`wire`]) | glibc offers only `getaddrinfo`'s hint, or `res_query` behind an unstable ABI |
//!
//! Android 7 to 9 have no typed query in the NDK, so previews there fail as
//! unavailable rather than fall back to a hinted lookup.
//!
//! [`resolve`] is the only caller of [`Dns::query`], and the tests in
//! `tests.rs` hold it to the two queries above with a recording [`Dns`], and
//! hold the wire client to them with a real UDP server. See `README.md` for
//! the packet-capture check on each platform.

pub(crate) mod message;
pub(crate) mod wire;

#[cfg(target_os = "android")]
mod android;
#[cfg(any(target_os = "macos", target_os = "ios"))]
mod apple;
#[cfg(windows)]
mod windows;

use crate::address::{Address, Nat64Prefix};
use crate::error::{Failure, Result};
use crate::Cancel;
use std::future::Future;
use std::net::IpAddr;
use std::pin::pin;
use tokio::time::Instant;

/// The name RFC 7050 discovery asks for.
pub(crate) const DISCOVERY: &str = "ipv4only.arpa";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RecordType {
    A,
    Aaaa,
}

impl RecordType {
    pub(crate) fn code(self) -> u16 {
        match self {
            RecordType::A => 1,
            RecordType::Aaaa => 28,
        }
    }
}

/// What one query found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    /// The first address the platform returned. Only the first is used, as
    /// the Swift reader's callback finishes on its first result.
    Address(IpAddr),
    /// An authoritative "no such name" or "no records of that type".
    NoRecord,
}

/// One typed query, answered by the platform. `name` has no trailing dot.
pub(crate) trait Dns: Send + Sync {
    fn query(
        &self,
        name: &str,
        record: RecordType,
        deadline: Instant,
        cancel: &Cancel,
    ) -> impl Future<Output = Result<Answer>> + Send;
}

/// The resolver for the platform this was built for.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) type System = apple::AppleDns;
#[cfg(target_os = "android")]
pub(crate) type System = android::AndroidDns;
#[cfg(windows)]
pub(crate) type System = windows::WindowsDns;
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "android", windows)))]
pub(crate) type System = wire::WireDns;

#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "android", windows)))]
pub(crate) fn system() -> System {
    wire::WireDns::from_system()
}
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) fn system() -> System {
    apple::AppleDns
}
#[cfg(target_os = "android")]
pub(crate) fn system() -> System {
    android::AndroidDns::load()
}
#[cfg(windows)]
pub(crate) fn system() -> System {
    windows::WindowsDns
}

/// `PreviewNetwork.resolve` and `resolvedAddresses`: the origin's first A
/// record, which must be public, and in front of it its NAT64 translation
/// when this network has an unambiguous prefix.
///
/// Both queries run at once inside one budget. The origin's answer is waited
/// for first; discovery only adds an address, so its failure or timeout never
/// fails the lookup. Cancellation always does.
pub(crate) async fn resolve<D: Dns>(
    dns: &D,
    host: &str,
    deadline: Instant,
    cancel: &Cancel,
) -> Result<Vec<Address>> {
    if cancel.is_cancelled() {
        return Err(Failure::Cancelled);
    }
    let mut origin = pin!(dns.query(host, RecordType::A, deadline, cancel));
    let mut probe = pin!(dns.query(DISCOVERY, RecordType::Aaaa, deadline, cancel));
    let mut probed = None;
    let ipv4 = loop {
        tokio::select! {
            answer = &mut origin => break answer,
            answer = &mut probe, if probed.is_none() => probed = Some(answer),
            () = tokio::time::sleep_until(deadline) => return Err(Failure::Expired),
        }
    }?;
    let ipv4 = match ipv4 {
        Answer::NoRecord => return Err(Failure::PublicIpv4Required),
        Answer::Address(ip) => Address::from(ip),
    };
    if !ipv4.is_ipv4() || !ipv4.is_public() {
        return Err(Failure::PreviewUnavailable);
    }
    let mut addresses = vec![ipv4];
    let probed = match probed {
        Some(answer) => answer,
        None => tokio::select! {
            answer = &mut probe => answer,
            () = tokio::time::sleep_until(deadline) => Err(Failure::Expired),
        },
    };
    // Optional discovery cannot invalidate a usable public IPv4 target, but
    // what it found is logged: on a network that should have NAT64, the
    // difference between "no record", "failed" and "no usable prefix" is the
    // whole diagnosis. No address is logged.
    let discovery = match probed {
        Err(Failure::Cancelled) => return Err(Failure::Cancelled),
        Ok(Answer::Address(ip @ IpAddr::V6(_))) => {
            match Nat64Prefix::discover(&Address::from(ip))
                .and_then(|prefix| Address::translated(&ipv4, &prefix))
            {
                Some(translated) => {
                    addresses.insert(0, translated);
                    "prefix".to_owned()
                }
                None => "unusable".to_owned(),
            }
        }
        Ok(Answer::Address(IpAddr::V4(_))) => "unusable".to_owned(),
        Ok(Answer::NoRecord) => "no_record".to_owned(),
        Err(failure) => format!("failed category={failure:?}"),
    };
    crate::log::event(&format!("device_preview_dns64 result={discovery}"));
    if cancel.is_cancelled() {
        return Err(Failure::Cancelled);
    }
    crate::log::event(&format!(
        "device_preview_dns_complete ipv6={} ipv4=true",
        addresses.len() == 2
    ));
    Ok(addresses)
}
