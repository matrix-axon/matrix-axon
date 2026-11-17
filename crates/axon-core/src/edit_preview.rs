//! Bundled link previews on an edit (ADR 0132 section 8).
//!
//! An edit's previews live in the replacement's `m.new_content`, which is what
//! the store and edit-aware clients show; the top-level `* ` fallback carries
//! an explicit `[]` so a client that shows it builds no preview of its own.
//! The request either omits the field, and the message's current previews are
//! carried forward, or sets it, where an entry may keep a current preview by
//! its link instead of resending it.
//!
//! Like [`link_preview`](crate::link_preview), nothing here fetches a URL.

use std::collections::HashSet;

use serde_json::{json, Value};

use crate::link_preview::{
    body_urls, bounded, check_content, install_previews, LinkPreview, PreviewError,
    MAX_IMAGE_BYTES, MAX_IMAGE_PIXELS, MAX_MEDIA_SOURCE_BYTES, MAX_MXC_LENGTH, MAX_PREVIEWS,
    MAX_PREVIEW_BYTES,
};
use crate::matrix_link::is_matrix_link;
use crate::Formatted;

/// One entry of an edit's explicit `url_previews` list.
pub enum EditPreview {
    /// Copy the message's current preview for this link, image included.
    Keep { matched_url: String },
    /// A new preview, exactly as on a send.
    New(LinkPreview),
}

/// One edit, as the gateway needs it.
pub struct PreviewEdit<'a> {
    pub body: &'a str,
    pub formatted: Option<Formatted<'a>>,
    /// `None`: carry the message's previews forward. `Some(vec![])`: no
    /// previews.
    pub previews: Option<Vec<EditPreview>>,
    /// The message's content as the store shows it, with its latest edit
    /// applied. `None` when the store does not have it; the gateway then uses
    /// the original event it fetches to check authorship.
    pub current: Option<Value>,
}

/// A `keep` entry named a link the message has no preview for at all, because
/// another device's edit removed it since this edit began. `409`.
#[derive(Debug, PartialEq, Eq)]
pub struct PreviewsChanged;

/// The message's previews as it stands, read the way the timeline reads them.
#[derive(Debug, Default, PartialEq)]
pub struct CurrentPreviews {
    /// Entries that pass the checks a new entry does, in the message's order,
    /// in the standard spelling: at most [`MAX_PREVIEWS`], one per link.
    entries: Vec<CurrentEntry>,
    /// Every link the message has an entry for, usable or not.
    named: HashSet<String>,
}

#[derive(Debug, PartialEq)]
struct CurrentEntry {
    /// The link as the entry spells it, which a body must contain verbatim.
    matched_url: String,
    /// The link's identity, for matching a `keep` entry.
    identity: String,
    value: Value,
}

/// The keys the server writes in a preview entry, and nothing else.
const ENTRY_KEYS: &[&str] = &[
    "matrix:matched_url",
    "og:title",
    "og:description",
    "og:site_name",
    "og:image",
    "matrix:image:encrypted",
    "og:image:type",
    "og:image:width",
    "og:image:height",
    "matrix:image:size",
];

/// The keys of the encrypted-file object the SDK writes.
const ENCRYPTED_FILE_KEYS: &[&str] = &["url", "key", "iv", "hashes", "v"];

/// The thumbnail types the upload path accepts.
const IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp"];

/// A link's identity: its `url` crate serialization, or the string itself when
/// it does not parse (such an entry fails the checks anyway).
fn identity(url: &str) -> String {
    url::Url::parse(url).map_or_else(|_| url.to_owned(), |u| u.to_string())
}

/// Read `content`'s preview field: `m.url_previews` when present, even when it
/// is empty or malformed, and a value that is not a list counts as `[]`;
/// otherwise `com.beeper.linkpreviews`, converted to the standard spelling.
/// `None` when the message has neither, which differs from `[]`.
pub fn current_previews(content: &Value) -> Option<CurrentPreviews> {
    let (field, alias) = match content.get("m.url_previews") {
        Some(field) => (field, false),
        None => (content.get("com.beeper.linkpreviews")?, true),
    };
    let mut current = CurrentPreviews::default();
    let Some(raw) = field.as_array() else {
        return Some(current);
    };
    for (index, entry) in raw.iter().enumerate() {
        let entry = if alias {
            from_alias(entry)
        } else {
            entry.clone()
        };
        let Some(matched_url) = entry
            .get("matrix:matched_url")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let identity = identity(&matched_url);
        current.named.insert(identity.clone());
        // Past the eighth, the most a send may carry, entries are left out as
        // failing ones are; a message with more came from another client.
        if index >= MAX_PREVIEWS
            || !entry_is_valid(&entry)
            || current.entries.iter().any(|e| e.identity == identity)
        {
            continue;
        }
        current.entries.push(CurrentEntry {
            matched_url,
            identity,
            value: entry,
        });
    }
    Some(current)
}

/// The compatibility spelling's renames, undone.
fn from_alias(entry: &Value) -> Value {
    let mut entry = entry.clone();
    if let Some(object) = entry.as_object_mut() {
        for (alias, standard) in [
            ("matched_url", "matrix:matched_url"),
            ("beeper:image:encryption", "matrix:image:encrypted"),
        ] {
            if let Some(value) = object.remove(alias) {
                object.insert(standard.to_owned(), value);
            }
        }
    }
    entry
}

/// Whether a current entry passes the checks a new one does: a matched link
/// within the send limits, text within its limits, an image that is a
/// well-formed `mxc://` URI or an encrypted-file object of the shape the
/// server writes, and only the keys the server itself writes. Whether its
/// media still exists is not checked.
fn entry_is_valid(entry: &Value) -> bool {
    let Some(object) = entry.as_object() else {
        return false;
    };
    if object.keys().any(|k| !ENTRY_KEYS.contains(&k.as_str())) {
        return false;
    }
    let text = |key: &str, limit: usize| {
        object
            .get(key)
            .is_none_or(|v| v.as_str().is_some_and(|s| bounded(s, limit)))
    };
    let Some(url) = object.get("matrix:matched_url").and_then(Value::as_str) else {
        return false;
    };
    // A Matrix link's preview is left out (ADR 0138 section 1): its card is
    // the recipient's to build, so an edit never carries one forward.
    let link_ok = bounded(url, 4096)
        && !is_matrix_link(url)
        && url::Url::parse(url).is_ok_and(|u| {
            matches!(u.scheme(), "http" | "https")
                && u.username().is_empty()
                && u.password().is_none()
        });
    let image_ok = match (object.get("og:image"), object.get("matrix:image:encrypted")) {
        (Some(_), Some(_)) => false,
        (Some(mxc), None) => mxc.as_str().is_some_and(mxc_is_valid),
        (None, Some(file)) => encrypted_file_is_valid(file),
        (None, None) => true,
    };
    let number = |key: &str, max: u64| {
        object
            .get(key)
            .is_none_or(|v| v.as_u64().is_some_and(|n| n > 0 && n <= max))
    };
    let pixels_ok = match (
        object.get("og:image:width").and_then(Value::as_u64),
        object.get("og:image:height").and_then(Value::as_u64),
    ) {
        (Some(w), Some(h)) => w.saturating_mul(h) <= MAX_IMAGE_PIXELS,
        _ => true,
    };
    link_ok
        && text("og:title", 300)
        && text("og:description", 1000)
        && text("og:site_name", 120)
        && image_ok
        && object
            .get("og:image:type")
            .is_none_or(|t| t.as_str().is_some_and(|t| IMAGE_TYPES.contains(&t)))
        && number("og:image:width", MAX_IMAGE_PIXELS)
        && number("og:image:height", MAX_IMAGE_PIXELS)
        && pixels_ok
        && number("matrix:image:size", MAX_IMAGE_BYTES)
}

fn mxc_is_valid(mxc: &str) -> bool {
    mxc.len() <= MAX_MXC_LENGTH && crate::media::parse_mxc(mxc).is_some()
}

fn encrypted_file_is_valid(file: &Value) -> bool {
    let Some(object) = file.as_object() else {
        return false;
    };
    object
        .keys()
        .all(|k| ENCRYPTED_FILE_KEYS.contains(&k.as_str()))
        && object
            .get("url")
            .and_then(Value::as_str)
            .is_some_and(mxc_is_valid)
        && object.get("key").is_some_and(Value::is_object)
        && object.get("iv").is_some_and(Value::is_string)
        && object
            .get("hashes")
            .and_then(|h| h.get("sha256"))
            .is_some_and(Value::is_string)
        && object.get("v").is_some_and(Value::is_string)
        && serde_json::to_vec(file).is_ok_and(|v| v.len() <= MAX_MEDIA_SOURCE_BYTES)
}

/// Whether HTML contains a spoiler (`data-mx-spoiler`), in any case, since
/// HTML attribute names are case-insensitive; the web renderer matches it the
/// same way. A false positive, such as the attribute's name written as text,
/// only sends `[]`.
pub fn has_spoiler(formatted_body: Option<&str>) -> bool {
    const SPOILER: &[u8] = b"data-mx-spoiler";
    formatted_body.is_some_and(|html| {
        html.as_bytes()
            .windows(SPOILER.len())
            .any(|w| w.eq_ignore_ascii_case(SPOILER))
    })
}

/// The previews an edit that omits the field carries forward, before the size
/// check. A message with no preview field gets none; one with a field keeps,
/// in order, the entries whose link is still a complete bare link in the new
/// body, and `[]` when none remain or the new text has a spoiler, so a message
/// that opted out stays opted out.
pub fn carry_forward(
    current: Option<&CurrentPreviews>,
    body: &str,
    formatted_body: Option<&str>,
) -> Option<Vec<Value>> {
    let current = current?;
    if has_spoiler(formatted_body) {
        return Some(Vec::new());
    }
    let urls = body_urls(body);
    Some(
        current
            .entries
            .iter()
            .filter(|e| urls.contains(e.matched_url.as_str()))
            .map(|e| e.value.clone())
            .collect(),
    )
}

/// The current entry a `keep` entry copies, its link spelled as the request
/// spells it (which the request's validation found in the new body). `Ok(None)`
/// when the message names the link but its entry fails the checks, which
/// leaves it out as carry-forward would; `Err` when the message has no entry
/// for the link at all.
pub fn resolve_keep(
    current: Option<&CurrentPreviews>,
    matched_url: &str,
) -> Result<Option<Value>, PreviewsChanged> {
    let current = current.ok_or(PreviewsChanged)?;
    let identity = identity(matched_url);
    if let Some(entry) = current.entries.iter().find(|e| e.identity == identity) {
        let mut value = entry.value.clone();
        value["matrix:matched_url"] = json!(matched_url);
        return Ok(Some(value));
    }
    if current.named.contains(&identity) {
        Ok(None)
    } else {
        Err(PreviewsChanged)
    }
}

/// Put `entries` in the replacement's `m.new_content`, both spellings, and an
/// explicit `[]` of each spelling in the top-level fallback.
pub fn install_edit_previews(replacement: &mut Value, entries: Vec<Value>) {
    if let Some(new_content) = replacement.get_mut("m.new_content") {
        install_previews(new_content, entries);
    }
    install_previews(replacement, Vec::new());
}

/// The size check for a replacement that carries previews: the preview budget
/// for `m.new_content`'s list, and the content budget for the whole event,
/// which carries the body twice.
pub fn check_replacement(replacement: &Value) -> Result<(), PreviewError> {
    if let Some(previews) = replacement
        .get("m.new_content")
        .and_then(|c| c.get("m.url_previews"))
    {
        if serde_json::to_vec(previews)
            .map_err(|_| PreviewError::Invalid)?
            .len()
            > MAX_PREVIEW_BYTES
        {
            return Err(PreviewError::PreviewsTooLarge);
        }
    }
    check_content(replacement)
}

/// Fit carried-forward entries to the size check, dropping whole entries from
/// the end, then sending `[]`, then no field. So a carried list never makes an
/// edit too large to send. A carried list is never smaller than `[]`, so when
/// `[]` does not fit, nothing carried does.
pub fn fit_carried(replacement: &Value, mut entries: Vec<Value>) -> Option<Vec<Value>> {
    loop {
        let mut candidate = replacement.clone();
        install_edit_previews(&mut candidate, entries.clone());
        if check_replacement(&candidate).is_ok() {
            return Some(entries);
        }
        entries.pop()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link_preview::MAX_CONTENT_BYTES;

    fn card(url: &str) -> Value {
        json!({"matrix:matched_url": url, "og:title": "Title"})
    }

    fn with_image(url: &str) -> Value {
        let mut value = card(url);
        value["og:image"] = json!("mxc://example.org/abc");
        value["og:image:type"] = json!("image/png");
        value["og:image:width"] = json!(10);
        value["og:image:height"] = json!(10);
        value["matrix:image:size"] = json!(100);
        value
    }

    fn encrypted(url: &str) -> Value {
        let mut value = card(url);
        value["matrix:image:encrypted"] = json!({
            "url": "mxc://example.org/enc",
            "key": {"kty": "oct", "key_ops": ["encrypt", "decrypt"], "alg": "A256CTR", "k": "abc", "ext": true},
            "iv": "AAAA",
            "hashes": {"sha256": "hash"},
            "v": "v2"
        });
        value
    }

    fn replacement(body: &str) -> Value {
        json!({
            "msgtype": "m.text",
            "body": format!("* {body}"),
            "m.new_content": {"msgtype": "m.text", "body": body},
            "m.relates_to": {"rel_type": "m.replace", "event_id": "$original"}
        })
    }

    fn values(current: &CurrentPreviews) -> Vec<Value> {
        current.entries.iter().map(|e| e.value.clone()).collect()
    }

    #[test]
    fn a_matrix_link_entry_is_left_out_and_never_carried_forward() {
        // ADR 0138 section 1: a stale matrix.to card, sent by another client or
        // before the ADR, goes on the first edit.
        let link = "https://matrix.to/#/!room:example.org/$event";
        let current = current_previews(&json!({"m.url_previews": [
            card(link),
            card("https://a.example/"),
        ]}))
        .unwrap();
        assert_eq!(values(&current), vec![card("https://a.example/")]);
        let body = format!("{link} https://a.example/");
        assert_eq!(
            carry_forward(Some(&current), &body, None),
            Some(vec![card("https://a.example/")])
        );
        // A keep of it is left out, as a failing entry's is: the message names
        // the link, so this is no `409`.
        assert_eq!(resolve_keep(Some(&current), link), Ok(None));

        // When it was the only entry, the message keeps its preview field, so
        // carry-forward sends `[]`.
        let only = current_previews(&json!({"m.url_previews": [card(link)]})).unwrap();
        assert_eq!(carry_forward(Some(&only), link, None), Some(vec![]));
    }

    #[test]
    fn no_field_is_not_an_empty_field() {
        assert_eq!(current_previews(&json!({"body": "x"})), None);
        let empty = current_previews(&json!({"m.url_previews": []})).unwrap();
        assert!(empty.entries.is_empty());
        assert_eq!(carry_forward(None, "x", None), None);
        assert_eq!(carry_forward(Some(&empty), "x", None), Some(vec![]));
    }

    #[test]
    fn the_standard_field_is_authoritative_even_when_malformed() {
        let content = json!({
            "m.url_previews": "not a list",
            "com.beeper.linkpreviews": [{"matched_url": "https://a.example/"}]
        });
        let current = current_previews(&content).unwrap();
        assert!(current.entries.is_empty());
        // The alias's link is not named: the message's field is the standard one.
        assert_eq!(
            resolve_keep(Some(&current), "https://a.example/"),
            Err(PreviewsChanged)
        );
    }

    #[test]
    fn the_alias_is_read_in_the_standard_spelling() {
        let content = json!({"com.beeper.linkpreviews": [{
            "matched_url": "https://a.example/",
            "beeper:image:encryption": encrypted("x")["matrix:image:encrypted"].clone()
        }]});
        let current = current_previews(&content).unwrap();
        let entry = &values(&current)[0];
        assert_eq!(entry["matrix:matched_url"], "https://a.example/");
        assert_eq!(
            entry["matrix:image:encrypted"]["url"],
            "mxc://example.org/enc"
        );
        assert!(entry.get("matched_url").is_none());
    }

    #[test]
    fn entries_the_server_would_not_write_are_left_out_but_named() {
        let bad = [
            json!({"matrix:matched_url": "https://a.example/", "og:url": "x"}),
            json!({"matrix:matched_url": "ftp://a.example/"}),
            json!({"matrix:matched_url": "https://u:p@a.example/"}),
            json!({"matrix:matched_url": "https://a.example/", "og:title": "t".repeat(301)}),
            json!({"matrix:matched_url": "https://a.example/", "og:image": "https://a.example/x.png"}),
            json!({"matrix:matched_url": "https://a.example/", "og:image": "mxc://example.org/a",
                   "matrix:image:encrypted": encrypted("x")["matrix:image:encrypted"].clone()}),
            json!({"matrix:matched_url": "https://a.example/", "og:image:type": "image/svg+xml"}),
            json!({"matrix:matched_url": "https://a.example/", "og:image:width": 0}),
            json!({"matrix:matched_url": "https://a.example/", "og:image:width": 4000, "og:image:height": 4000}),
            json!({"matrix:matched_url": "https://a.example/", "matrix:image:size": 3 * 1024 * 1024}),
            json!({"matrix:matched_url": "https://a.example/", "matrix:image:encrypted": {"url": "mxc://example.org/a"}}),
            json!({"matrix:matched_url": "https://a.example/", "matrix:image:encrypted": {
                "url": "mxc://example.org/a", "key": {}, "iv": "", "hashes": {"sha256": ""}, "v": "v2", "extra": 1}}),
        ];
        for entry in bad {
            let current = current_previews(&json!({"m.url_previews": [entry.clone()]})).unwrap();
            assert!(current.entries.is_empty(), "{entry}");
            let url = entry["matrix:matched_url"].as_str().unwrap();
            assert_eq!(resolve_keep(Some(&current), url), Ok(None), "{entry}");
        }
        for entry in [
            card("https://a.example/"),
            with_image("https://a.example/"),
            encrypted("https://a.example/"),
        ] {
            let current = current_previews(&json!({"m.url_previews": [entry.clone()]})).unwrap();
            assert_eq!(values(&current), vec![entry]);
        }
    }

    #[test]
    fn entries_past_the_eighth_are_left_out() {
        let entries: Vec<Value> = (0..10)
            .map(|n| card(&format!("https://a.example/{n}")))
            .collect();
        let current = current_previews(&json!({"m.url_previews": entries})).unwrap();
        assert_eq!(values(&current), entries[..8].to_vec());
        assert_eq!(
            resolve_keep(Some(&current), "https://a.example/9"),
            Ok(None)
        );
    }

    #[test]
    fn a_repeated_link_keeps_its_first_entry() {
        let mut second = card("https://a.example");
        second["og:title"] = json!("Second");
        let current =
            current_previews(&json!({"m.url_previews": [card("https://a.example/"), second]}))
                .unwrap();
        assert_eq!(values(&current), vec![card("https://a.example/")]);
    }

    #[test]
    fn carry_forward_keeps_links_still_in_the_body_in_order() {
        let current = current_previews(&json!({"m.url_previews": [
            card("https://a.example/"), card("https://b.example/"), card("https://c.example/")
        ]}))
        .unwrap();
        assert_eq!(
            carry_forward(
                Some(&current),
                "see https://c.example/ and https://a.example/.",
                None
            ),
            Some(vec![card("https://a.example/"), card("https://c.example/")])
        );
        // A link inside a longer one is not a match.
        assert_eq!(
            carry_forward(Some(&current), "https://a.example/more", None),
            Some(vec![])
        );
    }

    #[test]
    fn a_spoiler_sends_an_empty_list_only_for_a_message_with_a_field() {
        let html = Some(r#"<span data-mx-spoiler>https://a.example/</span>"#);
        let current =
            current_previews(&json!({"m.url_previews": [card("https://a.example/")]})).unwrap();
        assert_eq!(
            carry_forward(Some(&current), "https://a.example/", html),
            Some(vec![])
        );
        assert_eq!(carry_forward(None, "https://a.example/", html), None);
        // Attribute names are case-insensitive.
        let shouted = Some(r#"<span DATA-MX-Spoiler>https://a.example/</span>"#);
        assert_eq!(
            carry_forward(Some(&current), "https://a.example/", shouted),
            Some(vec![])
        );
        assert_eq!(
            carry_forward(
                Some(&current),
                "https://a.example/",
                Some("<b>https://a.example/</b>")
            ),
            Some(vec![card("https://a.example/")])
        );
    }

    #[test]
    fn keep_copies_the_current_entry_or_reports_a_change() {
        let current =
            current_previews(&json!({"m.url_previews": [with_image("https://a.example/")]}))
                .unwrap();
        let kept = resolve_keep(Some(&current), "HTTPS://A.example")
            .unwrap()
            .unwrap();
        assert_eq!(kept["og:image"], "mxc://example.org/abc");
        assert_eq!(kept["matrix:matched_url"], "HTTPS://A.example");
        assert_eq!(
            resolve_keep(Some(&current), "https://b.example/"),
            Err(PreviewsChanged)
        );
        assert_eq!(
            resolve_keep(None, "https://a.example/"),
            Err(PreviewsChanged)
        );
    }

    #[test]
    fn previews_go_in_new_content_with_an_empty_fallback() {
        let mut content = replacement("https://a.example/");
        install_edit_previews(&mut content, vec![card("https://a.example/")]);
        assert_eq!(
            content["m.new_content"]["m.url_previews"],
            json!([card("https://a.example/")])
        );
        assert_eq!(
            content["m.new_content"]["com.beeper.linkpreviews"][0]["matched_url"],
            "https://a.example/"
        );
        assert_eq!(content["m.url_previews"], json!([]));
        assert_eq!(content["com.beeper.linkpreviews"], json!([]));
        assert!(check_replacement(&content).is_ok());
    }

    #[test]
    fn the_size_check_counts_the_whole_replacement() {
        // Each copy of the body fits alone; the replacement carries two.
        let body = "x".repeat(MAX_CONTENT_BYTES / 2 + 100);
        let mut content = replacement(&body);
        install_edit_previews(&mut content, vec![]);
        assert_eq!(
            check_replacement(&content),
            Err(PreviewError::EventTooLarge)
        );
    }

    #[test]
    fn carried_entries_degrade_last_first_then_to_empty_then_to_no_field() {
        let long = |url: &str| {
            let mut value = card(url);
            value["og:description"] = json!("d".repeat(1000));
            value
        };
        let entries: Vec<Value> = (0..3)
            .map(|n| long(&format!("https://a.example/{n}")))
            .collect();
        let entry_bytes = serde_json::to_vec(&entries[0]).unwrap().len();
        let mut room = replacement("");
        install_edit_previews(&mut room, vec![]);
        let empty_bytes = serde_json::to_vec(&room).unwrap().len();
        // Fill the body so that exactly one entry still fits. Both spellings
        // count, so an entry costs a little over twice its bytes.
        let pad = MAX_CONTENT_BYTES - empty_bytes - 2 * entry_bytes - 60;
        let body = "x".repeat(pad / 2);
        let fitted = fit_carried(&replacement(&body), entries.clone()).unwrap();
        assert_eq!(fitted, entries[..1].to_vec());

        let body = "x".repeat((MAX_CONTENT_BYTES - empty_bytes) / 2 - 10);
        assert_eq!(
            fit_carried(&replacement(&body), entries.clone()),
            Some(vec![])
        );

        // When even [] does not fit, nothing carried does: a card the user
        // discarded is never sent again.
        let body = "x".repeat(MAX_CONTENT_BYTES / 2);
        assert_eq!(fit_carried(&replacement(&body), entries), None);
    }
}
