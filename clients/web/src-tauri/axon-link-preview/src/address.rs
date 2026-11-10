//! Which addresses a preview may connect to, and NAT64 (ADR 0132 § 3).
//!
//! Ported from `PreviewNetwork.Address` and `PreviewNAT64.swift`, rule for
//! rule. The cases are in `src/incoming-share/fixtures/preview-boundary.json`,
//! which the Swift tests read too.
//!
//! The rule that matters most: a global IPv6 address is never trusted on its
//! own, because it can be an operator's NAT64 mapping of a private IPv4
//! service. The only IPv6 address a preview connects to is one this process
//! synthesized from a public A record, with a prefix discovered through
//! RFC 7050. That provenance is [`Address::source_ipv4`], and nothing outside
//! this module can set it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Address {
    ip: IpAddr,
    /// The public IPv4 address an IPv6 address was synthesized from.
    source_ipv4: Option<Ipv4Addr>,
}

impl Address {
    /// A numeric address, as `inet_pton` reads it.
    pub(crate) fn parse(text: &str) -> Option<Address> {
        text.parse::<IpAddr>().ok().map(Address::from)
    }

    pub(crate) fn ip(&self) -> IpAddr {
        self.ip
    }

    pub(crate) fn is_ipv4(&self) -> bool {
        self.ip.is_ipv4()
    }

    /// `ipv4` placed in `prefix`, when both it and the result are public.
    pub(crate) fn translated(ipv4: &Address, prefix: &Nat64Prefix) -> Option<Address> {
        let IpAddr::V4(source) = ipv4.ip else {
            return None;
        };
        if !ipv4.is_public() {
            return None;
        }
        let candidate = Address {
            ip: IpAddr::V6(Ipv6Addr::from(prefix.embedding(source.octets()))),
            source_ipv4: Some(source),
        };
        candidate.is_public().then_some(candidate)
    }

    /// Whether a connection to this address is allowed: a public IPv4
    /// address, or an IPv6 address synthesized here from one.
    pub(crate) fn approved_for_connection(&self) -> bool {
        match self.ip {
            IpAddr::V4(_) => self.is_public(),
            IpAddr::V6(_) => match self.source_ipv4 {
                Some(source) => Address::from(IpAddr::V4(source)).is_public() && self.is_public(),
                None => false,
            },
        }
    }

    /// Outside every private, shared, loopback, link-local, documentation,
    /// benchmarking, multicast, reserved and transition range.
    pub(crate) fn is_public(&self) -> bool {
        match self.ip {
            IpAddr::V4(v4) => public_v4(v4.octets()),
            IpAddr::V6(v6) => {
                let b = v6.octets();
                // The well-known NAT64 prefix: only a public embedded address.
                if b[..12] == [0, 100, 255, 155, 0, 0, 0, 0, 0, 0, 0, 0] {
                    return public_v4([b[12], b[13], b[14], b[15]]);
                }
                if b[0] & 0xe0 != 0x20 {
                    return false;
                }
                // Special-purpose, transition, documentation and former 6bone
                // ranges.
                if b[0] == 0x20 && b[1] == 1 && (b[2] < 2 || (b[2] == 0x0d && b[3] == 0xb8)) {
                    return false;
                }
                if b[0] == 0x20 && b[1] == 2 {
                    return false;
                }
                if b[0] == 0x3f && (b[1] == 0xfe || (b[1] == 0xff && b[2] & 0xf0 == 0)) {
                    return false;
                }
                true
            }
        }
    }
}

impl From<IpAddr> for Address {
    fn from(ip: IpAddr) -> Self {
        Address {
            ip,
            source_ipv4: None,
        }
    }
}

fn public_v4(b: [u8; 4]) -> bool {
    if b[0] == 0 || b[0] >= 224 {
        return false;
    }
    if b[0] == 10 || b[0] == 127 {
        return false;
    }
    if b[0] == 100 && (64..=127).contains(&b[1]) {
        return false;
    }
    if b[0] == 169 && b[1] == 254 {
        return false;
    }
    if b[0] == 172 && (16..=31).contains(&b[1]) {
        return false;
    }
    if b[0] == 192
        && (b[1] == 168 || (b[1] == 0 && (b[2] == 0 || b[2] == 2)) || (b[1] == 88 && b[2] == 99))
    {
        return false;
    }
    if b[0] == 198 && (b[1] == 18 || b[1] == 19 || (b[1] == 51 && b[2] == 100)) {
        return false;
    }
    !(b[0] == 203 && b[1] == 0 && b[2] == 113)
}

/// An RFC 6052 prefix, discovered with RFC 7050. Used only to translate a
/// public IPv4 address this process already validated; it never authorizes an
/// origin's AAAA records, which are never asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Nat64Prefix {
    length: usize,
    bytes: [u8; 16],
}

impl Nat64Prefix {
    #[cfg(test)]
    pub(crate) fn length(&self) -> usize {
        self.length
    }

    /// The one prefix `probe`, an AAAA answer for `ipv4only.arpa`, encodes,
    /// or `None` when it encodes none or more than one.
    pub(crate) fn discover(probe: &Address) -> Option<Nat64Prefix> {
        let IpAddr::V6(v6) = probe.ip else {
            return None;
        };
        let probe = v6.octets();
        if probe[8] != 0 {
            return None;
        }
        const SENTINELS: [[u8; 4]; 2] = [[192, 0, 0, 170], [192, 0, 0, 171]];
        let check = Address::from(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)));
        let mut matches = Vec::new();
        for length in [32, 40, 48, 56, 64, 96] {
            let mut bytes = [0u8; 16];
            bytes[..length / 8].copy_from_slice(&probe[..length / 8]);
            let prefix = Nat64Prefix { length, bytes };
            // The complete canonical encoding, including the reserved `u`
            // octet and the zero suffix; malformed or ambiguous discovery data
            // is ignored.
            if SENTINELS
                .iter()
                .any(|sentinel| prefix.embedding(*sentinel) == probe)
                && Address::translated(&check, &prefix).is_some()
            {
                matches.push(prefix);
            }
        }
        match matches.as_slice() {
            [only] => Some(*only),
            _ => None,
        }
    }

    /// `ipv4` in this prefix, skipping the reserved octet 8.
    fn embedding(&self, ipv4: [u8; 4]) -> [u8; 16] {
        let mut result = self.bytes;
        let mut offset = self.length / 8;
        for byte in ipv4 {
            if offset == 8 {
                offset += 1;
            }
            result[offset] = byte;
            offset += 1;
        }
        result
    }
}
