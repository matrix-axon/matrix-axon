//! Bounded, wire-neutral bundled URL previews (ADR 0125 / MSC4095).
//!
//! No URL is fetched here. Limits use UTF-16 units for individual strings
//! (matching the web renderer), and canonical-JSON UTF-8 bytes for aggregates.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{Formatted, Relation};

pub const MAX_PREVIEWS: usize = 8;
pub const MAX_PREVIEW_BYTES: usize = 32 * 1024;
/// 40 KiB leaves >10 KiB after Megolm/base64 expansion for the federation
/// envelope, signatures and encryption fields beneath Matrix's 64 KiB cap.
pub const MAX_CONTENT_BYTES: usize = 40 * 1024;
pub const MAX_IMAGE_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_IMAGE_PIXELS: u64 = 4_000_000;
pub const MAX_MXC_LENGTH: usize = 1024;
/// Maximum serialized SDK media source (URI plus encryption metadata).
pub const MAX_MEDIA_SOURCE_BYTES: usize = 1024;
pub const IMAGE_RESERVE_BYTES: usize = MAX_MEDIA_SOURCE_BYTES + 256;
pub const MEDIA_RESERVE_BYTES: usize = 12 * 1024;

/// Public semantic metadata. API DTOs project onto this type after decoding.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewMetadata {
    pub matched_url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub site_name: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PreviewError {
    #[error("invalid link preview metadata")]
    Invalid,
    #[error("preview metadata exceeds the 32768-byte aggregate limit")]
    PreviewsTooLarge,
    #[error("message exceeds the 40960-byte serialized-content budget; shorten text or remove formatting/previews")]
    EventTooLarge,
}

/// An image whose bytes/type/dimensions have been checked by the upload port.
/// Intentionally does not derive Debug: bytes may contain private content.
pub struct PreviewImage {
    pub bytes: Vec<u8>,
    pub mimetype: String,
    pub width: u32,
    pub height: u32,
}

pub struct LinkPreview {
    pub metadata: PreviewMetadata,
    pub image: Option<PreviewImage>,
}

/// One new preview-aware send. `None` media means a text message; otherwise
/// `body` is its caption. An empty previews slice is an explicit opt-out.
pub struct PreviewSend<'a> {
    pub body: &'a str,
    pub formatted: Option<Formatted<'a>>,
    pub relation: Relation<'a>,
    pub previews: Vec<LinkPreview>,
}

fn bounded(value: &str, limit: usize) -> bool {
    value.encode_utf16().take(limit + 1).count() <= limit
}

/// Same bare-URL tokenization as clients/web/src/html/linkify.ts, including
/// trailing sentence punctuation. An embedded URL prefix is not a match.
fn body_urls(body: &str) -> HashSet<&str> {
    body.split(|c: char| c.is_whitespace() || matches!(c, '\u{feff}' | '<' | '>' | '"'))
        .filter_map(|token| {
            let start = ["http://", "https://", "matrix:"]
                .iter()
                .filter_map(|prefix| token.find(*prefix))
                .min()?;
            let url = &token[start..];
            if url.starts_with("matrix:") {
                return None;
            }
            Some(url.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '\'', '"']))
        })
        .collect()
}

pub fn validate_metadata(body: &str, previews: &[PreviewMetadata]) -> Result<(), PreviewError> {
    if previews.len() > MAX_PREVIEWS {
        return Err(PreviewError::Invalid);
    }
    if body.len() > 64 * 1024 {
        return Err(PreviewError::EventTooLarge);
    }
    let urls = body_urls(body);
    let mut seen = HashSet::new();
    for p in previews {
        if !bounded(&p.matched_url, 4096) {
            return Err(PreviewError::Invalid);
        }
        let url = url::Url::parse(&p.matched_url).map_err(|_| PreviewError::Invalid)?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || !urls.contains(p.matched_url.as_str())
            || !seen.insert(url.to_string())
            || p.title.as_deref().is_some_and(|s| !bounded(s, 300))
            || p.description.as_deref().is_some_and(|s| !bounded(s, 1000))
            || p.site_name.as_deref().is_some_and(|s| !bounded(s, 120))
        {
            return Err(PreviewError::Invalid);
        }
    }
    Ok(())
}

/// Build an MSC4095 entry. Image fields are supplied only by the SDK upload
/// path, never copied from untrusted client JSON.
pub fn entry(metadata: &PreviewMetadata) -> Value {
    let mut value = json!({"matrix:matched_url": metadata.matched_url});
    for (key, text) in [
        ("og:title", &metadata.title),
        ("og:description", &metadata.description),
        ("og:site_name", &metadata.site_name),
    ] {
        if let Some(text) = text {
            value[key] = json!(text);
        }
    }
    value
}

/// Allocation-free lookup across all retained entries, matching SQL containment.
/// Received events are not constrained by our outgoing card count. Standard metadata is
/// authoritative even when empty/malformed; never fall through to an alias.
pub fn preview_for_mxc<'a>(content: &'a Value, mxc: &str) -> Option<&'a Value> {
    let previews = content
        .get("m.url_previews")
        .or_else(|| content.get("com.beeper.linkpreviews"))?
        .as_array()?;
    // Prefer decryption evidence even if an earlier card repeats the same
    // URI as an ordinary image. A plaintext alias must not hide a known key.
    previews
        .iter()
        .find(|p| encrypted_preview_file_for_mxc(p, mxc).is_some())
        .or_else(|| {
            previews
                .iter()
                .find(|p| p.get("og:image").and_then(Value::as_str) == Some(mxc))
        })
}

pub fn encrypted_preview_file_for_mxc<'a>(preview: &'a Value, mxc: &str) -> Option<&'a Value> {
    ["matrix:image:encrypted", "beeper:image:encryption"]
        .into_iter()
        .filter_map(|key| preview.get(key))
        .find(|file| file.get("url").and_then(Value::as_str) == Some(mxc))
}

/// Publish both proposal and deployed compatibility spellings. The actual
/// event-size budget counts both copies, while the preview budget counts the
/// canonical logical array. An explicit empty array opts out in either client.
pub fn install_previews(content: &mut Value, entries: Vec<Value>) {
    let compat = entries
        .iter()
        .map(|p| {
            let mut p = p.clone();
            if let Some(object) = p.as_object_mut() {
                if let Some(url) = object.remove("matrix:matched_url") {
                    object.insert("matched_url".into(), url);
                }
                if let Some(file) = object.remove("matrix:image:encrypted") {
                    object.insert("beeper:image:encryption".into(), file);
                }
            }
            p
        })
        .collect::<Vec<_>>();
    content["m.url_previews"] = Value::Array(entries);
    content["com.beeper.linkpreviews"] = Value::Array(compat);
}

pub fn text_content(body: &str, formatted: Option<Formatted<'_>>, relation: Relation<'_>) -> Value {
    let mut content = json!({"msgtype": "m.text", "body": body});
    if let Some(f) = formatted {
        content["format"] = json!(f.format);
        content["formatted_body"] = json!(f.body);
    }
    if let Some(relation) = relation_content(relation) {
        content["m.relates_to"] = relation;
    }
    content
}

/// Thread membership never invents a reply fallback. SDK ID validation is
/// separate; this projection is also used by preflight before upload claims.
pub fn relation_content(relation: Relation<'_>) -> Option<Value> {
    if !relation.is_some() {
        return None;
    }
    let mut value = json!({});
    if let Some(root) = relation.thread_root {
        value["rel_type"] = json!("m.thread");
        value["event_id"] = json!(root);
    }
    if let Some(reply) = relation.reply_to {
        value["m.in_reply_to"] = json!({"event_id":reply});
    }
    Some(value)
}

/// Serialize the same JSON value ruma serializes; its strings/integers satisfy
/// canonical JSON. Object key order has no effect on byte length.
pub fn check_content(content: &Value) -> Result<(), PreviewError> {
    if let Some(previews) = content.get("m.url_previews") {
        if serde_json::to_vec(previews)
            .map_err(|_| PreviewError::Invalid)?
            .len()
            > MAX_PREVIEW_BYTES
        {
            return Err(PreviewError::PreviewsTooLarge);
        }
    }
    if serde_json::to_vec(content)
        .map_err(|_| PreviewError::Invalid)?
        .len()
        > MAX_CONTENT_BYTES
    {
        return Err(PreviewError::EventTooLarge);
    }
    Ok(())
}

/// Conservative preflight BEFORE claiming any upload. Reserve bounded MXC,
/// crypto descriptors and media metadata so SDK-produced fields cannot turn
/// an accepted draft into an oversized event. Actual content is checked too.
pub fn preflight(
    body: &str,
    formatted: Option<Formatted<'_>>,
    relation: Relation<'_>,
    previews: &[(PreviewMetadata, bool)],
    media: bool,
) -> Result<(), PreviewError> {
    validate_metadata(
        body,
        &previews.iter().map(|(m, _)| m.clone()).collect::<Vec<_>>(),
    )?;
    let mut content = text_content(body, formatted, relation);
    install_previews(
        &mut content,
        previews
            .iter()
            .map(|(m, image)| {
                let mut value = entry(m);
                if *image {
                    value["matrix:image:encrypted"] = json!("x".repeat(IMAGE_RESERVE_BYTES));
                }
                value
            })
            .collect(),
    );
    if media {
        content["_reserved_media_fields"] = json!("x".repeat(MEDIA_RESERVE_BYTES));
    }
    check_content(&content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_image_cards_fit_a_short_caption_with_media_reserve() {
        let previews = (0..MAX_PREVIEWS)
            .map(|n| {
                (
                    PreviewMetadata {
                        matched_url: format!("https://example.org/{n}"),
                        title: Some("Example".into()),
                        ..Default::default()
                    },
                    true,
                )
            })
            .collect::<Vec<_>>();
        let body = previews
            .iter()
            .map(|(p, _)| p.matched_url.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            preflight(&body, None, Relation::default(), &previews, true),
            Ok(())
        );
    }

    #[test]
    fn compatibility_keys_and_standard_authority_are_preserved() {
        let mut content = json!({});
        install_previews(
            &mut content,
            vec![
                json!({"matrix:matched_url":"https://example.org","matrix:image:encrypted":{"url":"mxc://example.org/x"}}),
            ],
        );
        assert_eq!(
            content["com.beeper.linkpreviews"][0]["matched_url"],
            "https://example.org"
        );
        assert!(content["com.beeper.linkpreviews"][0]
            .get("matrix:matched_url")
            .is_none());
        let preview = &content["com.beeper.linkpreviews"][0];
        assert_eq!(
            preview["beeper:image:encryption"]["url"],
            "mxc://example.org/x"
        );
        content.as_object_mut().unwrap().remove("m.url_previews");
        assert!(preview_for_mxc(&content, "mxc://example.org/x").is_some());
        content["m.url_previews"] = json!([]);
        assert!(preview_for_mxc(&content, "mxc://example.org/x").is_none());
    }

    #[test]
    fn matches_whole_link_tokens_and_rejects_credentials() {
        for url in ["https://example.org", "https://user:secret@example.org"] {
            let p = PreviewMetadata {
                matched_url: url.into(),
                ..Default::default()
            };
            assert_eq!(
                validate_metadata(&format!("See {url}."), std::slice::from_ref(&p)).is_ok(),
                !url.contains("secret")
            );
            assert!(validate_metadata("https://example.org/longer", &[p]).is_err());
        }
    }

    #[test]
    fn measures_escaped_bytes_and_combined_content() {
        let p = PreviewMetadata {
            matched_url: "https://example.org".into(),
            description: Some("\u{0001}".repeat(1000)),
            ..Default::default()
        };
        let values = (0..8)
            .map(|n| {
                let mut p = p.clone();
                p.matched_url = format!("https://example.org/{n}");
                (p, false)
            })
            .collect::<Vec<_>>();
        let body = values
            .iter()
            .map(|(p, _)| p.matched_url.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            preflight(&body, None, Relation::default(), &values, false),
            Err(PreviewError::PreviewsTooLarge)
        );
        assert_eq!(
            preflight(
                &"x".repeat(41 * 1024),
                None,
                Relation::default(),
                &[],
                false
            ),
            Err(PreviewError::EventTooLarge)
        );
        assert!(preflight("hello", None, Relation::default(), &[], false).is_ok());
    }
}
