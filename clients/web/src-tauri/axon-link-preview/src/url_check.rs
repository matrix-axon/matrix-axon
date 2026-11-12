//! The static check, repeated natively for the first URL and for every
//! redirect and image URL (ADR 0132 § 1 and § 3).
//!
//! Ported from `PreviewNetwork.validate` and `TextShare.previewURL` in the
//! Swift reader, and kept equal to `nativePreviewLinks` in
//! `src/incoming-share/urls.ts`, which is what decides that the composer shows
//! a card at all. All three read `src/incoming-share/fixtures/device-preview-urls.json`.
//!
//! Two places where the parsers differ, and what keeps the rule the same:
//!
//! - The `url` crate follows the WHATWG URL standard, as `new URL` does in the
//!   composer. Foundation's `URLComponents` does not. So a URL is first held to
//!   the characters and shape both accept, and only then parsed.
//! - Foundation decodes a punycode host to Unicode, which the ASCII-only host
//!   rule then rejects. The `url` crate keeps it as `xn--`, so an `xn--` label
//!   is rejected explicitly, as the TypeScript check does.

use crate::address::Address;
use crate::error::{Failure, Result};
use url::Url;

/// Top-level labels that never name a public site.
const RESERVED: [&str; 16] = [
    "local",
    "localhost",
    "home",
    "lan",
    "corp",
    "internal",
    "localdomain",
    "intranet",
    "private",
    "onion",
    "i2p",
    "invalid",
    "test",
    "example",
    "alt",
    "arpa",
];

/// The characters `TextShare.portableURLCharacters` allows: RFC 3986's
/// unreserved and reserved sets, and `%`.
fn portable(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-._~:/?#[]@!$&'()*+,;=%".contains(&byte)
}

/// A `%` not followed by two hex digits.
fn invalid_percent(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.iter().enumerate().any(|(index, &byte)| {
        byte == b'%'
            && !(bytes.get(index + 1).is_some_and(u8::is_ascii_hexdigit)
                && bytes.get(index + 2).is_some_and(u8::is_ascii_hexdigit))
    })
}

/// A URL that passed, with the host it was validated for: lower case, without
/// a trailing dot. That is the name DNS is asked for and TLS verifies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Checked {
    pub(crate) url: Url,
    pub(crate) host: String,
}

/// `PreviewNetwork.validate`: accept the URL or fail with `PreviewUnavailable`.
pub(crate) fn validate(raw: &str) -> Result<Checked> {
    check(raw).ok_or(Failure::PreviewUnavailable)
}

fn check(raw: &str) -> Option<Checked> {
    // `TextShare.validURL` and `previewURL`. The portable set already excludes
    // whitespace and control characters, and every non-ASCII character, so
    // the byte length is the UTF-16 length here.
    if raw.len() > 4096 || !raw.bytes().all(portable) || invalid_percent(raw) {
        return None;
    }
    // `https://` with its slashes. WHATWG would also read `https:host` and
    // `https:/host` as a host; Foundation reads them as a path with no host.
    if !raw
        .get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
    {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let host = match url.host()? {
        url::Host::Domain(domain) => domain.to_ascii_lowercase(),
        url::Host::Ipv4(_) | url::Host::Ipv6(_) => return None,
    };
    let host = host.strip_suffix('.').unwrap_or(&host).to_owned();
    let labels: Vec<&str> = host.split('.').collect();
    let ok = !host.is_empty()
        && host.len() <= 253
        && labels.len() > 1
        && host.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-.".contains(&byte)
        })
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && !label.starts_with("xn--")
        })
        && labels
            .last()
            .is_some_and(|last| !last.bytes().all(|byte| byte.is_ascii_digit()))
        && Address::parse(&host).is_none()
        && url.port().is_none_or(|port| port >= 1)
        && labels.last().is_some_and(|last| !RESERVED.contains(last));
    ok.then_some(Checked { url, host })
}

/// The request target: the path, `/` for an empty one, and the query.
pub(crate) fn target(url: &Url) -> String {
    let path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };
    match url.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    }
}

/// The `Host` header: the validated host, and the port when one was given.
pub(crate) fn authority(url: &Url, host: &str) -> String {
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

/// The port to connect to.
pub(crate) fn port(url: &Url) -> u16 {
    url.port().unwrap_or(443)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_is_lowercased_without_its_trailing_dot() {
        let checked = validate("https://Page.Example.ORG./a").unwrap();
        assert_eq!(checked.host, "page.example.org");
    }

    #[test]
    fn scheme_needs_its_slashes() {
        for raw in [
            "https:example.org/a",
            "https:/example.org/a",
            "HTTPS://example.org/a",
        ] {
            assert_eq!(validate(raw).is_ok(), raw.starts_with("HTTPS://"), "{raw}");
        }
    }

    #[test]
    fn target_and_authority_are_what_the_swift_reader_sends() {
        let url = Url::parse("https://example.org:8443/a%20b?q=1#frag").unwrap();
        assert_eq!(target(&url), "/a%20b?q=1");
        assert_eq!(authority(&url, "example.org"), "example.org:8443");
        assert_eq!(port(&url), 8443);
        let url = Url::parse("https://example.org").unwrap();
        assert_eq!(target(&url), "/");
        assert_eq!(authority(&url, "example.org"), "example.org");
        assert_eq!(port(&url), 443);
        let url = Url::parse("https://example.org/?").unwrap();
        assert_eq!(target(&url), "/?");
    }
}
