//! Why a request failed, reduced to words that are safe to log (ADR 0127 § 4).
//!
//! Nothing here forwards error text. `reqwest::Error::without_url` strips only
//! the error's own URL, and the DNS and TLS errors further down the chain can
//! still name the host. Traces get pasted into bug reports, so what crosses the
//! IPC is a fixed `kind` and a `detail` taken from a closed set of names.

use std::error::Error as StdError;
use std::fmt;
use std::io;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Dns,
    Refused,
    ConnectTimeout,
    Tls,
    Reset,
    Body,
    /// A cancel the transport saw: the caller's own `cancel`, or the webview
    /// going away with the request in flight. The binding turns the first into
    /// the caller's `AbortError`, so only the second reaches `api:failed`.
    Cancelled,
    /// A URL, or a redirect hop, whose scheme is not `http` or `https`.
    Scope,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransportError {
    pub kind: Kind,
    /// `None`, an `io::ErrorKind` name, or a name from [`tls_detail`]. Never
    /// built from an error's text.
    pub detail: Option<String>,
}

impl TransportError {
    pub(crate) fn new(kind: Kind) -> Self {
        Self { kind, detail: None }
    }

    pub(crate) fn with(kind: Kind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: Some(detail.into()),
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            Some(detail) => write!(f, "{:?} ({detail})", self.kind),
            None => write!(f, "{:?}", self.kind),
        }
    }
}

impl StdError for TransportError {}

/// Name resolution failed. Raised by the client's resolver so that `dns` is
/// recognised by type: `hyper-util`'s own resolver error is private and says
/// only "dns error".
#[derive(Debug)]
pub(crate) struct DnsFailure(pub(crate) io::Error);

impl fmt::Display for DnsFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("name resolution failed")
    }
}

impl StdError for DnsFailure {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&self.0)
    }
}

/// Every error in the chain, starting with `err` itself.
///
/// `io::Error::source` returns its wrapped error's *source*, skipping the
/// wrapped error, and that wrapped error is where rustls puts its own. So an
/// `io::Error` is followed through `get_ref` instead.
fn causes<'a>(err: &'a (dyn StdError + 'static)) -> Vec<&'a (dyn StdError + 'static)> {
    let mut out = Vec::new();
    let mut next = Some(err);
    while let Some(err) = next {
        out.push(err);
        if out.len() > 32 {
            break;
        }
        next = match err.downcast_ref::<io::Error>() {
            Some(io) => io.get_ref().map(|inner| inner as &(dyn StdError + 'static)),
            None => err.source(),
        };
    }
    out
}

fn find<'a, T: StdError + 'static>(causes: &[&'a (dyn StdError + 'static)]) -> Option<&'a T> {
    causes.iter().find_map(|err| err.downcast_ref::<T>())
}

fn io_kind_name(kind: io::ErrorKind) -> String {
    // `ErrorKind`'s `Debug` is its variant name: a closed set defined by std.
    format!("{kind:?}")
}

fn is_reset_io(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
    )
}

/// The certificate or alert behind a TLS failure, by variant name. The
/// `…Context` variants carry the names a certificate was presented for, so
/// each one is matched out explicitly rather than printed.
// `UnsupportedSignatureAlgorithm` is deprecated in favour of its `…Context`
// form, but older verifiers still raise it.
#[allow(deprecated)]
fn tls_detail(err: &rustls::Error) -> Option<&'static str> {
    use rustls::{AlertDescription as A, CertificateError as C, Error as E};
    Some(match err {
        E::InvalidCertificate(cert) => match cert {
            C::BadEncoding => "BadEncoding",
            C::Expired | C::ExpiredContext { .. } => "Expired",
            C::NotValidYet | C::NotValidYetContext { .. } => "NotValidYet",
            C::Revoked => "Revoked",
            C::UnknownIssuer => "UnknownIssuer",
            C::BadSignature => "BadSignature",
            C::NotValidForName | C::NotValidForNameContext { .. } => "NotValidForName",
            C::InvalidPurpose | C::InvalidPurposeContext { .. } => "InvalidPurpose",
            C::UnsupportedSignatureAlgorithm
            | C::UnsupportedSignatureAlgorithmContext { .. }
            | C::UnsupportedSignatureAlgorithmForPublicKeyContext { .. } => {
                "UnsupportedSignatureAlgorithm"
            }
            _ => "InvalidCertificate",
        },
        E::AlertReceived(alert) => match alert {
            A::HandshakeFailure => "HandshakeFailure",
            A::ProtocolVersion => "ProtocolVersion",
            A::UnrecognisedName => "UnrecognisedName",
            A::NoApplicationProtocol => "NoApplicationProtocol",
            A::CertificateRequired => "CertificateRequired",
            A::InternalError => "InternalError",
            _ => "AlertReceived",
        },
        E::NoApplicationProtocol => "NoApplicationProtocol",
        E::PeerIncompatible(_) => "PeerIncompatible",
        E::PeerMisbehaved(_) => "PeerMisbehaved",
        E::InvalidMessage(_) => "InvalidMessage",
        _ => return None,
    })
}

/// HTTP/2 told us the server did not process the stream (RFC 9113 § 8.7), so
/// sending it again is safe for any method. The two checks reqwest's private
/// default policy makes, written out so each retry can be counted.
pub(crate) fn is_protocol_nack(err: &(dyn StdError + 'static)) -> bool {
    let causes = causes(err);
    let Some(h2) = find::<h2::Error>(&causes) else {
        return false;
    };
    let graceful_goaway =
        h2.is_go_away() && h2.is_remote() && h2.reason() == Some(h2::Reason::NO_ERROR);
    let refused =
        h2.is_reset() && h2.is_remote() && h2.reason() == Some(h2::Reason::REFUSED_STREAM);
    graceful_goaway || refused
}

/// The category of a failure before a response arrived.
pub(crate) fn classify_send(err: &reqwest::Error) -> TransportError {
    let causes = causes(err);
    if find::<DnsFailure>(&causes).is_some() {
        return TransportError::new(Kind::Dns);
    }
    if let Some(tls) = find::<rustls::Error>(&causes) {
        return match tls_detail(tls) {
            Some(detail) => TransportError::with(Kind::Tls, detail),
            None => TransportError::new(Kind::Tls),
        };
    }
    // No request or read timeout is configured, so a timeout here is the
    // connect timeout, or the OS giving up on the handshake (`ETIMEDOUT`).
    if err.is_timeout() {
        return if err.is_connect() {
            TransportError::new(Kind::ConnectTimeout)
        } else {
            TransportError::with(Kind::Other, "TimedOut")
        };
    }
    if let Some(io) = find::<io::Error>(&causes) {
        let kind = io.kind();
        if kind == io::ErrorKind::ConnectionRefused {
            return TransportError::with(Kind::Refused, io_kind_name(kind));
        }
        if kind == io::ErrorKind::TimedOut && err.is_connect() {
            return TransportError::new(Kind::ConnectTimeout);
        }
        if is_reset_io(kind) {
            return TransportError::with(Kind::Reset, io_kind_name(kind));
        }
    }
    if is_reset(&causes) {
        return TransportError::new(Kind::Reset);
    }
    TransportError::new(Kind::Other)
}

/// The connection went away under the request without an `io::Error` to say
/// so: the server closed after reading it, or reset the HTTP/2 stream or
/// connection.
fn is_reset(causes: &[&(dyn StdError + 'static)]) -> bool {
    if let Some(hyper) = find::<hyper::Error>(causes) {
        if hyper.is_incomplete_message() || hyper.is_canceled() {
            return true;
        }
    }
    if let Some(h2) = find::<h2::Error>(causes) {
        if h2.is_reset() || h2.is_go_away() {
            return true;
        }
        if let Some(io) = h2.get_io() {
            return is_reset_io(io.kind());
        }
    }
    if let Some(io) = find::<io::Error>(causes) {
        return is_reset_io(io.kind());
    }
    false
}

/// Whether a send failure is the connection going away, the one case the
/// send path retries itself for `GET` and `HEAD` (ADR 0127 § 7).
pub(crate) fn is_reset_failure(err: &reqwest::Error) -> bool {
    classify_send(err).kind == Kind::Reset
}

/// The category of a failure while reading a body that had started.
pub(crate) fn classify_body(err: &reqwest::Error) -> TransportError {
    let causes = causes(err);
    match find::<io::Error>(&causes) {
        Some(io) => TransportError::with(Kind::Body, io_kind_name(io.kind())),
        None => TransportError::new(Kind::Body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialises_as_kind_and_detail() {
        let err = TransportError::with(Kind::ConnectTimeout, "TimedOut");
        assert_eq!(
            serde_json::to_value(&err).unwrap(),
            serde_json::json!({ "kind": "connect_timeout", "detail": "TimedOut" })
        );
        let err = TransportError::new(Kind::Dns);
        assert_eq!(
            serde_json::to_value(&err).unwrap(),
            serde_json::json!({ "kind": "dns", "detail": null })
        );
    }

    #[test]
    fn causes_look_inside_io_errors() {
        let tls = rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer);
        let io = io::Error::new(io::ErrorKind::InvalidData, tls);
        let causes = causes(&io);
        assert_eq!(
            find::<rustls::Error>(&causes).and_then(tls_detail),
            Some("UnknownIssuer")
        );
    }
}
