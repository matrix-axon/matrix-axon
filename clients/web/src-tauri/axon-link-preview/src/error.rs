//! Failures, in two layers that mirror the Swift reader.
//!
//! [`Failure`] is the transport's own vocabulary, `ShareFailure` in
//! `incoming-share`. [`Kind`] is what the page sees, `DevicePreviewFailure`
//! there, plus `busy` and `cancelled`. The mapping between them is
//! [`Kind::from`], and it is the one the Swift adapter makes in
//! `DevicePreview.include`.

use serde::Serialize;

/// What went wrong inside a lookup. Never carries a host, URL or address, so
/// nothing here can put the link the user typed into a log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The URL, a redirect target, an image URL, or a resolved address is
    /// outside the boundary, or the page answered with something unusable.
    PreviewUnavailable,
    /// The origin has no A record: an IPv6-only site, or a name that does
    /// not exist. No backend tells the two apart (issue 1059), so the page's
    /// copy names both.
    PublicIpv4Required,
    /// The response broke HTTP framing.
    InvalidResponse,
    /// A response, header block or parse input was larger than its budget.
    Quota,
    /// The network or the resolver failed.
    Unavailable,
    /// The shared deadline passed.
    Expired,
    /// The page cancelled the lookup.
    Cancelled,
    /// An image could not be decoded or re-encoded.
    Invalid,
}

pub(crate) type Result<T> = std::result::Result<T, Failure>;

/// The rejection the page receives, as `{ "kind": "..." }`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The link is not one this device may look up.
    UnsupportedUrl,
    /// The site has no IPv4 address: it is IPv6-only, or the name does not
    /// exist.
    PublicIpv4Required,
    /// The 25-second deadline passed.
    Timeout,
    /// The page had no usable title, description, site name or image.
    NoMetadata,
    /// Anything else: the network, the resolver, or the page.
    Unavailable,
    /// Too many lookups are registered at once.
    Busy,
    /// The page cancelled it.
    Cancelled,
}

impl From<Failure> for Kind {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Expired => Kind::Timeout,
            Failure::PublicIpv4Required => Kind::PublicIpv4Required,
            Failure::Cancelled => Kind::Cancelled,
            Failure::PreviewUnavailable
            | Failure::InvalidResponse
            | Failure::Quota
            | Failure::Unavailable
            | Failure::Invalid => Kind::Unavailable,
        }
    }
}

/// The error a command rejects with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct LookupError {
    pub kind: Kind,
}

impl From<Kind> for LookupError {
    fn from(kind: Kind) -> Self {
        LookupError { kind }
    }
}

impl From<Failure> for LookupError {
    fn from(failure: Failure) -> Self {
        Kind::from(failure).into()
    }
}

impl std::fmt::Display for LookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "link preview failed: {:?}", self.kind)
    }
}

impl std::error::Error for LookupError {}
