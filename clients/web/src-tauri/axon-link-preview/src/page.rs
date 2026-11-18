//! Preview metadata from a page's head, ported from `RestrictedPreview.parse`.
//!
//! The page is parsed into an inert tree: html5ever runs no scripts and loads
//! nothing, and nothing here follows a URL except the one image the caller
//! fetches through the same restricted transport.
//!
//! The Swift reader asks SwiftSoup for `document.select(selector).first()`,
//! one selector after another, and takes the first value the send contract
//! accepts. This reproduces that: for each selector, the first `<meta>` in
//! document order whose attribute matches (trimmed, ignoring ASCII case, as
//! SwiftSoup's `AttributeWithValue` does), with its `content` checked by
//! [`preview_field`] before trimming, as `TextShare.previewField` does. The
//! cases both readers must agree on are in
//! `src/incoming-share/fixtures/preview-boundary.json`.

use crate::error::{Failure, Result};
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};

/// The send contract's limits, in UTF-16 code units.
pub(crate) const TITLE_LIMIT: usize = 300;
pub(crate) const DESCRIPTION_LIMIT: usize = 1000;
pub(crate) const SITE_NAME_LIMIT: usize = 120;
const IMAGE_URL_LIMIT: usize = 4096;
/// Initial HTML body budget, and the hard cap for the optional continuation.
pub(crate) const INITIAL_HEAD_BYTES: usize = 256 * 1024;
pub(crate) const MAX_HEAD_BYTES: usize = 1024 * 1024;
/// Only the bytes before the 513th `<` are parsed, which bounds the tree.
pub(crate) const MAX_TAGS: usize = 512;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Page {
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) site_name: Option<String>,
    pub(crate) image: Option<String>,
}

/// `TextShare.previewField`: a value longer than `limit` UTF-16 code units,
/// before or after trimming, or empty after it, is dropped, never truncated.
pub(crate) fn preview_field(value: Option<&str>, limit: usize) -> Option<String> {
    let value = value?;
    if value.encode_utf16().count() > limit {
        return None;
    }
    let trimmed = foundation_trim(value);
    (!trimmed.is_empty() && trimmed.encode_utf16().count() <= limit).then(|| trimmed.to_owned())
}

/// `trimmingCharacters(in: .whitespacesAndNewlines)`. Foundation's set is
/// Unicode's `White_Space` plus U+200B ZERO WIDTH SPACE, measured on macOS
/// by enumerating the set.
pub(crate) fn foundation_trim(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || c == '\u{200B}')
}

/// Bytes to text, as the Swift reader decodes them: UTF-8 when valid, then
/// Windows-1252 when the response declared Latin-1 or Windows-1252 and every
/// byte is defined there, and lossy UTF-8 otherwise.
pub(crate) fn decode(data: &[u8], content_type: &str) -> String {
    if let Ok(text) = std::str::from_utf8(data) {
        return text.to_owned();
    }
    if content_type.contains("iso-8859-1") || content_type.contains("windows-1252") {
        if let Some(text) = windows_1252(data) {
            return text;
        }
    }
    String::from_utf8_lossy(data).into_owned()
}

/// Windows-1252, or `None` for the five bytes it leaves undefined, which is
/// when Foundation's conversion returns `nil`.
fn windows_1252(data: &[u8]) -> Option<String> {
    const HIGH: [u16; 32] = [
        0x20AC, 0, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039,
        0x0152, 0, 0x017D, 0, 0, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC,
        0x2122, 0x0161, 0x203A, 0x0153, 0, 0x017E, 0x0178,
    ];
    data.iter()
        .map(|&byte| match byte {
            0x80..=0x9F => match HIGH[usize::from(byte - 0x80)] {
                0 => None,
                code => char::from_u32(u32::from(code)),
            },
            _ => Some(char::from(byte)),
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn parse(data: &[u8], content_type: &str) -> Result<Page> {
    parse_body(data, content_type, false)
}

pub(crate) fn parse_body(data: &[u8], content_type: &str, cutoff: bool) -> Result<Page> {
    if data.len() > MAX_HEAD_BYTES {
        return Err(Failure::Quota);
    }
    // Bound the tree, and its recursive destruction, before building it: a
    // head with more `<` than that is parsed only up to the first one past
    // the bound. Preview tags come early in a head (on 36 measured pages, all
    // of them within the first 160 `<`), so a heavy page keeps its metadata
    // rather than losing its card. A tag the cut ends is dropped whole.
    let end = tag_bound(data);
    let html = decode(&data[..end], content_type);
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(html);
    let mut elements = Vec::new();
    collect_elements(&dom.document, &mut elements);
    let content = |selectors: &[(&str, &str)], limit: usize| {
        selectors.iter().find_map(|(attribute, value)| {
            let node = elements
                .iter()
                .find(|node| is_meta_with(node, attribute, value))?;
            preview_field(
                Some(&attribute_of(node, "content").unwrap_or_default()),
                limit,
            )
        })
    };
    let title = content(
        &[("property", "og:title"), ("name", "twitter:title")],
        TITLE_LIMIT,
    )
    .or_else(|| {
        let title = preview_field(Some(&document_title(&elements)), TITLE_LIMIT)?;
        (!(cutoff || end < data.len()) || head_probe(&data[..end], false).complete_title)
            .then_some(title)
    });
    let page = Page {
        title,
        description: content(
            &[
                ("property", "og:description"),
                ("name", "twitter:description"),
                ("name", "description"),
            ],
            DESCRIPTION_LIMIT,
        ),
        site_name: content(&[("property", "og:site_name")], SITE_NAME_LIMIT),
        image: content(
            &[
                ("property", "og:image:secure_url"),
                ("property", "og:image"),
                ("name", "twitter:image"),
                ("name", "twitter:image:src"),
            ],
            IMAGE_URL_LIMIT,
        ),
    };
    Ok(page)
}

/// Where the bytes allowed to reach the parser end: just before the
/// `MAX_TAGS + 1`th `<`, or the end of the data.
fn tag_bound(data: &[u8]) -> usize {
    data.iter()
        .enumerate()
        .filter(|(_, &byte)| byte == b'<')
        .nth(MAX_TAGS)
        .map_or(data.len(), |(index, _)| index)
}

/// At the checkpoint use a small fragment of complete meta tags. The full
/// document goes through the tree builder only once, after collecting it.
pub(crate) fn has_rich_metadata(data: &[u8], content_type: &str) -> Result<bool> {
    let probe = head_probe(data, true);
    if probe.open_title || probe.meta.is_empty() {
        return Ok(false);
    }
    let page = parse_body(&probe.meta, content_type, false)?;
    Ok(page.description.is_some() || page.image.is_some())
}

#[derive(Default)]
struct HeadProbe {
    meta: Vec<u8>,
    complete_title: bool,
    open_title: bool,
}

/// A conservative head scan. The HTML parser still owns field selection and
/// attribute decoding. Skip raw text and comments so they cannot supply hints.
fn head_probe(mut input: &[u8], collect_meta: bool) -> HeadProbe {
    const PROBE_BYTES: usize = 16 * 1024;
    let mut probe = HeadProbe::default();
    let mut raw: Option<&[u8]> = None;
    while let Some(start) = input.iter().position(|&byte| byte == b'<') {
        input = &input[start..];
        if let Some(name) = raw {
            // Escaped script states cannot be inferred by this scan.
            if name == b"script" && input.starts_with(b"<!--") {
                break;
            }
            let end = 2 + name.len();
            if !input.starts_with(b"</")
                || !input
                    .get(2..end)
                    .is_some_and(|tag| tag.eq_ignore_ascii_case(name))
                || !input.get(end).is_some_and(|&byte| tag_boundary(byte))
            {
                input = &input[1..];
                continue;
            }
            let Some(end) = tag_end(input) else { break };
            if name == b"title" {
                probe.complete_title = true;
                probe.open_title = false;
            }
            raw = None;
            input = &input[end..];
            continue;
        }
        if input.starts_with(b"<!--") {
            let Some(end) = input[4..].windows(3).position(|bytes| bytes == b"-->") else {
                break;
            };
            input = &input[end + 7..];
            continue;
        }
        let Some(end) = tag_end(input) else { break };
        let name_end = input[1..end]
            .iter()
            .position(|&byte| tag_boundary(byte))
            .unwrap()
            + 1;
        let name = &input[1..name_end];
        if name.eq_ignore_ascii_case(b"meta") {
            if collect_meta {
                if probe.meta.len() + end > PROBE_BYTES {
                    probe.meta.clear();
                    break;
                }
                probe.meta.extend_from_slice(&input[..end]);
            }
        } else if name.eq_ignore_ascii_case(b"title") {
            raw = Some(b"title");
            probe.open_title = true;
        } else if let Some(name) = [b"script".as_slice(), b"style", b"noframes", b"noscript"]
            .into_iter()
            .find(|expected| expected.eq_ignore_ascii_case(name))
        {
            raw = Some(name);
        } else if ![
            b"html".as_slice(),
            b"head",
            b"link",
            b"base",
            b"basefont",
            b"bgsound",
            b"!doctype",
        ]
        .iter()
        .any(|expected| expected.eq_ignore_ascii_case(name))
        {
            // The scanner cannot interpret template, foreign or body elements.
            break;
        }
        input = &input[end..];
    }
    probe
}

fn tag_boundary(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ' | b'/' | b'>')
}

/// Position after an unquoted closing bracket.
fn tag_end(tag: &[u8]) -> Option<usize> {
    let mut quote = None;
    for (index, &byte) in tag.iter().enumerate().skip(1) {
        match (quote, byte) {
            (None, b'\'' | b'"') => quote = Some(byte),
            (Some(open), close) if open == close => quote = None,
            (None, b'>') => return Some(index + 1),
            _ => {}
        }
    }
    None
}

/// Every element, in document order. Iterative, so a deep tree cannot
/// exhaust the stack.
fn collect_elements(root: &Handle, into: &mut Vec<Handle>) {
    let mut stack = vec![root.clone()];
    while let Some(node) = stack.pop() {
        if matches!(node.data, NodeData::Element { .. }) {
            into.push(node.clone());
        }
        let children = node.children.borrow();
        stack.extend(children.iter().rev().cloned());
    }
}

fn local_name(node: &Handle) -> Option<String> {
    match &node.data {
        NodeData::Element { name, .. } => Some(name.local.to_string()),
        _ => None,
    }
}

fn attribute_of(node: &Handle, attribute: &str) -> Option<String> {
    match &node.data {
        NodeData::Element { attrs, .. } => attrs
            .borrow()
            .iter()
            .find(|attr| attr.name.local.as_ref() == attribute)
            .map(|attr| attr.value.to_string()),
        _ => None,
    }
}

/// `meta[attribute=value]`, as SwiftSoup matches it.
fn is_meta_with(node: &Handle, attribute: &str, value: &str) -> bool {
    local_name(node).as_deref() == Some("meta")
        && attribute_of(node, attribute)
            .is_some_and(|actual| byte_trim(&actual).eq_ignore_ascii_case(value))
}

/// The byte trim SwiftSoup applies to an attribute value before comparing
/// it: space, and U+0009 to U+000D.
fn byte_trim(value: &str) -> &str {
    value.trim_matches([' ', '\t', '\n', '\u{0B}', '\u{0C}', '\r'])
}

/// SwiftSoup's `Document.title()`: the first `<title>`'s text with each run
/// of whitespace collapsed to one space, then trimmed as Foundation
/// trims.
fn document_title(elements: &[Handle]) -> String {
    let Some(title) = elements
        .iter()
        .find(|node| local_name(node).as_deref() == Some("title"))
    else {
        return String::new();
    };
    let mut text = String::new();
    collect_text(title, &mut text);
    foundation_trim(&normalise_whitespace(&text)).to_owned()
}

fn collect_text(node: &Handle, into: &mut String) {
    let mut stack = vec![node.clone()];
    while let Some(node) = stack.pop() {
        if let NodeData::Text { contents } = &node.data {
            into.push_str(&contents.borrow());
        }
        stack.extend(node.children.borrow().iter().rev().cloned());
    }
}

/// What SwiftSoup collapses in a title (`Character.isWhitespace` in its
/// `CharacterExt.swift`): space, tab, line feed, form feed and carriage
/// return. Not a vertical tab, and not a no-break space, which only the final
/// trim removes, and only at either end.
fn actually_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\u{0C}' | '\r')
}

fn normalise_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut last_was_white = false;
    for c in text.chars() {
        if actually_whitespace(c) {
            if !last_was_white {
                result.push(' ');
            }
            last_was_white = true;
        } else {
            result.push(c);
            last_was_white = false;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_inert_metadata_and_decodes_entities() {
        let page = parse(
            b"<html><head><title>Fallback</title><meta property='og:title' content='A &amp; B'><meta name=description content='Details &quot;quoted&quot;'><meta property='og:site_name' content='Example'><meta property='og:image' content='/photo.png'><script src='https://127.0.0.1/script'></script><link rel=stylesheet href='https://127.0.0.1/style'></head></html>",
            "text/html",
        )
        .unwrap();
        assert_eq!(page.title.as_deref(), Some("A & B"));
        assert_eq!(page.description.as_deref(), Some("Details \"quoted\""));
        assert_eq!(page.site_name.as_deref(), Some("Example"));
        assert_eq!(page.image.as_deref(), Some("/photo.png"));
        // Past the bound, nothing more is parsed.
        let heavy = format!(
            "<title>Early</title>{}<meta property=og:title content=Late>",
            "<x>".repeat(600)
        );
        let page = parse(heavy.as_bytes(), "text/html").unwrap();
        assert_eq!(page.title.as_deref(), Some("Early"));
        assert_eq!(
            tag_bound(heavy.as_bytes()),
            heavy.match_indices('<').nth(512).unwrap().0
        );
        assert_eq!(tag_bound(b"<a><b>"), 6);
    }

    #[test]
    fn preview_field_checks_the_limit_before_trimming() {
        let padded = format!(" {}", "a".repeat(300));
        assert_eq!(preview_field(Some(&padded), 300), None);
        assert_eq!(preview_field(Some("  a  "), 300).as_deref(), Some("a"));
        assert_eq!(preview_field(Some(" \u{A0}\u{2003} "), 300), None);
        assert_eq!(
            preview_field(Some("\u{200B}x\u{200B}"), 300).as_deref(),
            Some("x")
        );
        // UTF-16 code units, not bytes or scalars.
        assert_eq!(
            preview_field(Some(&"😀".repeat(150)), 300).map(|v| v.len()),
            Some(600)
        );
        assert_eq!(preview_field(Some(&"😀".repeat(151)), 300), None);
    }

    #[test]
    fn windows_1252_leaves_five_bytes_undefined() {
        assert_eq!(windows_1252(&[0x41, 0x80]).as_deref(), Some("A€"));
        assert_eq!(windows_1252(&[0x41, 0x9F]).as_deref(), Some("AŸ"));
        assert_eq!(windows_1252(&[0x41, 0xE9]).as_deref(), Some("Aé"));
        for undefined in [0x81, 0x8D, 0x8F, 0x90, 0x9D] {
            assert_eq!(windows_1252(&[0x41, undefined]), None);
        }
        assert_eq!(
            decode(&[0x41, 0x81, 0xE9], "text/html; charset=windows-1252"),
            "A\u{FFFD}\u{FFFD}"
        );
        assert_eq!(decode(&[0x41, 0xE9], "text/html"), "A\u{FFFD}");
    }
}
