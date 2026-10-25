//! Untrusted homeserver preview normalization; no network activity.
use crate::link_preview::{validate_metadata, PreviewMetadata};
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr};
use url::{Host, Url};

/// Public HTTP(S) destinations only. DNS resolution and origin-fetch SSRF
/// protection belong to the homeserver, which alone contacts the webpage.
/// Preserve the exact matched input for the settled send contract.
pub fn eligible_url(input: &str) -> bool {
    if input.len() > 16384
        || input.encode_utf16().count() > 4096
        || input.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return false;
    }
    let Ok(url) = Url::parse(input) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    match url.host() {
        Some(Host::Ipv4(ip)) => public_v4(ip),
        Some(Host::Ipv6(ip)) => {
            // Accept global unicast only; exclude special-purpose 2001::/23,
            // documentation and 6to4 (which can embed private IPv4 addresses).
            let s = ip.segments();
            ip.to_ipv4_mapped().is_none()
                && s[0] & 0xe000 == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x200 || s[1] == 0xdb8))
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
        Some(Host::Domain(host)) => {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            host.contains('.')
                && ![
                    "localhost",
                    "local",
                    "internal",
                    "home",
                    "lan",
                    "test",
                    "invalid",
                    "onion",
                    "arpa",
                ]
                .iter()
                .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
                && host.parse::<IpAddr>().is_err()
        }
        None => false,
    }
}
fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !matches!(
        (a, b, c),
        (0 | 10 | 127 | 224..=255, _, _)
            | (100, 64..=127, _)
            | (169, 254, _)
            | (172, 16..=31, _)
            | (192, 168, _)
            | (192, 0, 0 | 2)
            | (192, 88, 99)
            | (198, 18 | 19, _)
            | (198, 51, 100)
            | (203, 0, 113)
    )
}

/// Retain only bounded, plain text fields. Never copy HTML or arbitrary OG
/// keys, trust declared image dimensions, or replace the requested URL.
pub fn normalize(url: &str, value: &Value) -> Option<PreviewMetadata> {
    if !eligible_url(url) || !value.is_object() {
        return None;
    }
    let text = |key, limit| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| {
                !s.is_empty()
                    && s.encode_utf16().count() <= limit
                    && !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
            })
            .map(str::to_owned)
    };
    let p = PreviewMetadata {
        matched_url: url.into(),
        title: text("og:title", 300),
        description: text("og:description", 1000),
        site_name: text("og:site_name", 120),
    };
    validate_metadata(url, std::slice::from_ref(&p)).ok()?;
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_urls_and_normalized_allow_list() {
        for url in [
            "https://example.org/page",
            "http://8.8.8.8/",
            "http://192.0.73.2/",
            "http://192.2.1.1/",
            "https://[2606:4700:4700::1111]/",
        ] {
            assert!(eligible_url(url), "{url}");
        }
        for url in [
            "file:///secret",
            "https://user:secret@example.org",
            "http://127.1",
            "http://2130706433",
            "http://0x7f000001",
            "https://10.0.0.1",
            "https://169.254.169.254",
            "https://100.64.0.1",
            "https://192.0.0.1",
            "https://192.0.2.1",
            "https://192.168.0.1",
            "https://192.88.99.1",
            "https://198.51.100.1",
            "https://203.0.113.1",
            "https://[::1]",
            "https://[::ffff:127.0.0.1]",
            "https://[2002:7f00:1::]",
            "https://LOCALHOST.",
            "https://foo.local",
            "http://intranet",
            " https://example.org",
            "https://example.org/\n",
        ] {
            assert!(!eligible_url(url), "{url}");
        }
        let p = normalize("https://example.org", &serde_json::json!({"og:title":"  title  ", "og:description":false, "og:site_name":"x".repeat(121), "og:url":"https://evil.org", "html":"<script>"})).unwrap();
        assert_eq!(p.title.as_deref(), Some("title"));
        assert_eq!(p.description, None);
        assert_eq!(p.site_name, None);
        assert_eq!(p.matched_url, "https://example.org");
        assert!(normalize("https://example.org", &serde_json::json!([])).is_none());
        assert!(!eligible_url(&format!(
            "https://example.org/{}",
            "a".repeat(4096)
        )));
    }
}
