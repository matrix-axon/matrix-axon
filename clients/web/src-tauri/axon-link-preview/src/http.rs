//! A bounded HTTP/1.1 response collector, ported from `PreviewHTTPParser.swift`.
//!
//! `httparse` reads each header block; this handles body framing. A page is
//! collected only until its `<head>` ends, or `<body` starts, or 256 KiB. An
//! image needs its complete body, at most 2 MiB. Compressed responses, folded
//! headers, conflicting framing and more than five interim responses are all
//! refused.

use crate::error::{Failure, Result};
use std::collections::HashMap;

/// A page is read until its head ends, or this many bytes.
pub(crate) const HEAD_BYTES: usize = 256 * 1024;
/// The response head, including every interim response.
pub(crate) const HEADER_BYTES: usize = 32 * 1024;
/// An image, as the server's upload contract allows (`ShareLimits.thumbnailBytes`).
pub(crate) const THUMBNAIL_BYTES: usize = 2 * 1024 * 1024;
/// The most one read may hand the parser, as the Swift reader asks for.
pub(crate) const READ_BYTES: usize = 16 * 1024;
/// More than this many header lines is refused rather than parsed.
const MAX_HEADERS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Page,
    Image,
}

impl Kind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Kind::Page => "page",
            Kind::Image => "image",
        }
    }
}

pub(crate) fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

#[derive(Debug)]
pub(crate) struct Parser {
    kind: Kind,
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
    pending: Vec<u8>,
    length: Option<usize>,
    chunked: bool,
    chunk: Option<usize>,
    chunks: usize,
    ending_chunks: bool,
    interim: usize,
    finished: bool,
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// `</head` or `<body`, in any case, followed by what ends an HTML tag name:
/// whitespace, `/` or `>`. So `<bodyguard>` does not end a head. One inside
/// script or comment text still does; the reader does not tokenize. The Swift
/// reader (`PreviewHTTPParser.endsHead`) applies the same rule.
pub(crate) fn ends_head(bytes: &[u8]) -> bool {
    [&b"</head"[..], &b"<body"[..]].iter().any(|marker| {
        bytes.windows(marker.len() + 1).any(|window| {
            window[..marker.len()].eq_ignore_ascii_case(marker)
                && matches!(
                    window[marker.len()],
                    b'\t' | b'\n' | 0x0C | b'\r' | b' ' | b'/' | b'>'
                )
        })
    })
}

/// Header bytes as ISO-8859-1, which is what a header field is.
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| char::from(byte)).collect()
}

impl Parser {
    pub(crate) fn new(kind: Kind) -> Self {
        Parser {
            kind,
            status: 0,
            headers: HashMap::new(),
            body: Vec::new(),
            pending: Vec::new(),
            length: None,
            chunked: false,
            chunk: None,
            chunks: 0,
            ending_chunks: false,
            interim: 0,
            finished: false,
        }
    }

    pub(crate) fn status(&self) -> u16 {
        self.status
    }

    pub(crate) fn into_parts(self) -> (u16, HashMap<String, String>, Vec<u8>) {
        (self.status, self.headers, self.body)
    }

    /// A page that is really an image is held to the image's rules.
    fn prefix(&self) -> bool {
        self.kind == Kind::Page
            && !self
                .headers
                .get("content-type")
                .map(|value| value.to_ascii_lowercase())
                .unwrap_or_default()
                .starts_with("image/")
    }

    fn limit(&self) -> usize {
        if self.prefix() {
            HEAD_BYTES
        } else {
            THUMBNAIL_BYTES
        }
    }

    /// Feed `data`; `true` once the response is complete enough to use.
    pub(crate) fn append(&mut self, data: &[u8], eof: bool) -> Result<bool> {
        if self.finished {
            return Ok(true);
        }
        if data.len() > READ_BYTES {
            return Err(Failure::Quota);
        }
        self.pending.extend_from_slice(data);
        while self.status == 0 {
            let Some(end) = find(&self.pending, b"\r\n\r\n") else {
                if self.pending.len() > HEADER_BYTES || eof {
                    return Err(Failure::InvalidResponse);
                }
                return Ok(false);
            };
            let count = end + 4;
            if count > HEADER_BYTES {
                return Err(Failure::Quota);
            }
            let header: Vec<u8> = self.pending.drain(..count).collect();
            if self.head(&header)? {
                return Ok(true);
            }
        }
        if self.chunked {
            return self.chunks(eof);
        }
        let wanted = self
            .pending
            .len()
            .min(self.length.map_or(self.pending.len(), |length| {
                length.saturating_sub(self.body.len())
            }));
        if wanted > 0 {
            let data: Vec<u8> = self.pending[..wanted].to_vec();
            let consumed = self.collect(&data)?;
            self.pending.drain(..consumed);
        }
        if self.finished {
            return Ok(true);
        }
        if self.length == Some(self.body.len()) {
            self.finished = true;
            return Ok(true);
        }
        if eof {
            if self.length.is_some() {
                return Err(Failure::InvalidResponse);
            }
            self.finished = true;
            return Ok(true);
        }
        Ok(false)
    }

    /// One header block. `true` when it ends the response (a redirect).
    fn head(&mut self, header: &[u8]) -> Result<bool> {
        let mut fields = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut response = httparse::Response::new(&mut fields);
        // Folded headers are an error by default, as the Swift reader makes
        // them one.
        match response.parse(header) {
            Ok(httparse::Status::Complete(_)) => {}
            _ => return Err(Failure::InvalidResponse),
        }
        let code = response.code.ok_or(Failure::InvalidResponse)?;
        if !(100..=599).contains(&code) || code == 101 {
            return Err(Failure::InvalidResponse);
        }
        if code < 200 {
            self.interim += 1;
            if self.interim > 5 {
                return Err(Failure::InvalidResponse);
            }
            return Ok(false);
        }
        let mut headers: HashMap<String, String> = HashMap::new();
        let mut lengths = 0;
        let mut transfers = 0;
        for field in response.headers.iter() {
            let name = field.name.to_ascii_lowercase();
            // `CharacterSet.whitespaces`, as far as ISO-8859-1 reaches.
            let value = latin1(field.value)
                .trim_matches([' ', '\t', '\u{A0}'])
                .to_owned();
            lengths += usize::from(name == "content-length");
            transfers += usize::from(name == "transfer-encoding");
            // CFNetwork joins a repeated field with a comma.
            headers
                .entry(name)
                .and_modify(|joined| {
                    joined.push_str(", ");
                    joined.push_str(&value);
                })
                .or_insert(value);
        }
        // Ambiguous framing.
        if lengths > 1 || transfers > 1 {
            return Err(Failure::InvalidResponse);
        }
        self.status = code;
        self.headers = headers;
        if is_redirect(code) {
            self.finished = true;
            return Ok(true);
        }
        if code != 200 {
            return Err(Failure::PreviewUnavailable);
        }
        if self
            .headers
            .get("content-encoding")
            .is_some_and(|encoding| !encoding.eq_ignore_ascii_case("identity"))
        {
            return Err(Failure::InvalidResponse);
        }
        if let Some(transfer) = self.headers.get("transfer-encoding") {
            if !transfer.eq_ignore_ascii_case("chunked")
                || self.headers.contains_key("content-length")
            {
                return Err(Failure::InvalidResponse);
            }
            self.chunked = true;
        } else if let Some(raw) = self.headers.get("content-length") {
            if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(Failure::InvalidResponse);
            }
            // Swift's `Int(_:)` fails past `Int.max`; so does this.
            let value: usize = raw
                .parse::<i64>()
                .ok()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or(Failure::InvalidResponse)?;
            if !self.prefix() && value > self.limit() {
                return Err(Failure::Quota);
            }
            self.length = Some(value);
        }
        Ok(false)
    }

    fn chunks(&mut self, eof: bool) -> Result<bool> {
        loop {
            if self.ending_chunks {
                if self.pending.starts_with(b"\r\n") {
                    self.finished = true;
                    return Ok(true);
                }
                if let Some(end) = find(&self.pending, b"\r\n\r\n") {
                    if end + 4 > HEADER_BYTES {
                        return Err(Failure::Quota);
                    }
                    self.finished = true;
                    return Ok(true);
                }
                if self.pending.len() > HEADER_BYTES || eof {
                    return Err(Failure::InvalidResponse);
                }
                return Ok(false);
            }
            if self.chunk.is_none() {
                let Some(end) = find(&self.pending, b"\r\n") else {
                    if self.pending.len() > 1024 || eof {
                        return Err(Failure::InvalidResponse);
                    }
                    return Ok(false);
                };
                if end > 1024 {
                    return Err(Failure::InvalidResponse);
                }
                let line = &self.pending[..end];
                if !line.is_ascii() {
                    return Err(Failure::InvalidResponse);
                }
                let size_text = line.split(|&byte| byte == b';').next().unwrap_or_default();
                if size_text.is_empty()
                    || size_text.len() > 16
                    || !size_text.iter().all(u8::is_ascii_hexdigit)
                {
                    return Err(Failure::InvalidResponse);
                }
                // Sixteen hex digits can exceed `Int.max`, which Swift's
                // `Int(_:radix:)` refuses.
                let size = std::str::from_utf8(size_text)
                    .ok()
                    .and_then(|text| i64::from_str_radix(text, 16).ok())
                    .and_then(|size| usize::try_from(size).ok())
                    .ok_or(Failure::InvalidResponse)?;
                self.pending.drain(..end + 2);
                self.chunks += 1;
                if self.chunks > 4096 {
                    return Err(Failure::Quota);
                }
                if size == 0 {
                    self.ending_chunks = true;
                    continue;
                }
                if !self.prefix() && size > self.limit() - self.body.len() {
                    return Err(Failure::Quota);
                }
                self.chunk = Some(size);
            }
            let remaining = self.chunk.unwrap_or_default();
            let wanted = remaining.min(self.pending.len());
            if wanted > 0 {
                let data: Vec<u8> = self.pending[..wanted].to_vec();
                let consumed = self.collect(&data)?;
                self.pending.drain(..consumed);
                if self.finished {
                    return Ok(true);
                }
                self.chunk = Some(remaining - consumed);
            }
            if self.chunk.is_some_and(|left| left > 0) {
                if eof {
                    return Err(Failure::InvalidResponse);
                }
                return Ok(false);
            }
            if self.pending.len() < 2 {
                if eof {
                    return Err(Failure::InvalidResponse);
                }
                return Ok(false);
            }
            if !self.pending.starts_with(b"\r\n") {
                return Err(Failure::InvalidResponse);
            }
            self.pending.drain(..2);
            self.chunk = None;
        }
    }

    fn collect(&mut self, data: &[u8]) -> Result<usize> {
        let prefix = self.prefix();
        let wanted = data.len().min(self.limit() - self.body.len());
        if !prefix && wanted != data.len() {
            return Err(Failure::Quota);
        }
        self.body.extend_from_slice(&data[..wanted]);
        if prefix {
            // Only the new tail, so a large head is not rescanned per read.
            let start = self.body.len().saturating_sub(wanted + 8);
            let tail = &self.body[start..];
            if ends_head(tail) || self.body.len() == HEAD_BYTES {
                self.finished = true;
            }
        }
        Ok(wanted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_with_split_chunks_and_interim_headers() {
        let mut parser = Parser::new(Kind::Image);
        let response = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: image/png\r\n\r\n3\r\nabc\r\n2;test=yes\r\nde\r\n0\r\n\r\n";
        let mut done = false;
        for byte in response {
            done = parser.append(&[*byte], false).unwrap();
        }
        assert!(done);
        assert_eq!(parser.into_parts().2, b"abcde");
    }

    #[test]
    fn oversize_and_malformed_responses_fail_closed() {
        for raw in [
            "HTTP/1.1 200 OK\r\nContent-Length: 9223372036854775807\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nx",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 1\r\n\r\nx",
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\n\r\nx",
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabc",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nFFFFFFFFFFFFFFFF\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n",
            "HTTP/1.1 200 OK\r\nX-Folded: a\r\n b\r\n\r\n",
            "HTTP/1.1 101 Switching Protocols\r\n\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
        ] {
            let mut parser = Parser::new(Kind::Image);
            assert!(parser.append(raw.as_bytes(), true).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn html_stops_at_head_and_images_require_complete_bodies() {
        let mut parser = Parser::new(Kind::Page);
        assert!(parser
            .append(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 99999999\r\n\r\n<title>Title</title></head>ignored body",
                false,
            )
            .unwrap());
        let body = parser.into_parts().2;
        assert!(String::from_utf8_lossy(&body).contains("Title"));
        let mut image = Parser::new(Kind::Image);
        assert!(!image
            .append(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabc", false)
            .unwrap());
        assert!(image.append(b"", true).is_err());
    }

    #[test]
    fn a_page_that_is_an_image_is_held_to_the_image_limit() {
        let mut parser = Parser::new(Kind::Page);
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: IMAGE/png\r\nContent-Length: {}\r\n\r\n",
            THUMBNAIL_BYTES + 1
        );
        assert_eq!(parser.append(head.as_bytes(), false), Err(Failure::Quota));
    }

    #[test]
    fn head_scan_sees_a_marker_split_across_reads() {
        let mut parser = Parser::new(Kind::Page);
        assert!(!parser
            .append(b"HTTP/1.1 200 OK\r\n\r\n<title>T</title></he", false)
            .unwrap());
        assert!(parser.append(b"ad>", false).unwrap());
    }

    #[test]
    fn a_head_end_split_across_reads_is_found_once_its_boundary_arrives() {
        // The cases themselves are the shared fixture's `headEnds`.
        let mut parser = Parser::new(Kind::Page);
        assert!(!parser
            .append(
                b"HTTP/1.1 200 OK\r\n\r\n<title>T</title><bodyguard><body",
                false
            )
            .unwrap());
        assert!(parser.append(b">", false).unwrap());
    }

    #[test]
    fn redirects_finish_at_their_head_and_other_statuses_fail() {
        let mut parser = Parser::new(Kind::Page);
        assert!(parser
            .append(b"HTTP/1.1 302 Found\r\nLocation: /next\r\n\r\n", false)
            .unwrap());
        let (status, headers, _) = parser.into_parts();
        assert_eq!(status, 302);
        assert_eq!(headers["location"], "/next");
        let mut parser = Parser::new(Kind::Page);
        assert_eq!(
            parser.append(b"HTTP/1.1 404 Not Found\r\n\r\n", false),
            Err(Failure::PreviewUnavailable)
        );
    }

    #[test]
    fn a_close_delimited_body_ends_at_eof() {
        let mut parser = Parser::new(Kind::Image);
        assert!(!parser.append(b"HTTP/1.1 200 OK\r\n\r\nabc", false).unwrap());
        assert!(parser.append(b"", true).unwrap());
        assert_eq!(parser.into_parts().2, b"abc");
    }

    #[test]
    fn too_many_interim_responses_fail() {
        let mut parser = Parser::new(Kind::Page);
        let raw = "HTTP/1.1 103 Early Hints\r\n\r\n".repeat(6);
        assert_eq!(
            parser.append(raw.as_bytes(), false),
            Err(Failure::InvalidResponse)
        );
    }
}
