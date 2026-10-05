//! Opaque timeline-pagination cursor.
//!
//! The store's [`TimelineCursor`] is a `(origin_ts, id)` sort key with public
//! fields. We don't expose those on the wire — instead a cursor is the
//! base64url (no padding) of `"{origin_ts}.{id}"`, so the on-the-wire contract
//! stays fixed even if the internal sort key changes.
//!
//! `.` is an unambiguous separator here: both fields are `i64`, whose decimal
//! form is ASCII digits with at most a leading `-` — never a `.` — so
//! `split_once('.')` always cleaves the two fields cleanly (negatives included).

use axon_store::TimelineCursor;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

/// Upper bound on the encoded length of any cursor we'll even attempt to decode.
/// Every cursor this module emits is the base64url of a short decimal string, so
/// anything materially longer is malformed (or hostile) — reject it before we
/// spend work base64-decoding it. Universal hygiene applied by every decoder here.
const MAX_ENCODED_LEN: usize = 64;

/// Encode a sort key into an opaque cursor string.
pub fn encode(cursor: TimelineCursor) -> String {
    URL_SAFE_NO_PAD.encode(format!("{}.{}", cursor.origin_ts, cursor.id))
}

/// Construct a synthetic cursor positioned just before or at the given
/// timestamp. Using `id = i64::MAX` as the tiebreaker means the store's
/// `(origin_ts, id) < (ts, MAX)` comparison includes every row whose
/// `origin_ts` equals `ts`, since real BIGSERIAL ids never reach `i64::MAX`.
pub fn from_ts(origin_ts: i64) -> TimelineCursor {
    TimelineCursor {
        origin_ts,
        id: i64::MAX,
    }
}

/// Decode an opaque cursor string back into a sort key. Returns `None` for any
/// malformed input (not base64, not UTF-8, wrong shape, unparsable integers) so
/// the handler can map it to a `400`.
pub fn decode(s: &str) -> Option<TimelineCursor> {
    if s.len() > MAX_ENCODED_LEN {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(s).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (ts, id) = text.split_once('.')?;
    Some(TimelineCursor {
        origin_ts: ts.parse().ok()?,
        id: id.parse().ok()?,
    })
}

/// Encode a search offset into an opaque cursor string. Search results are
/// relevance- or timestamp-ordered using offset pagination, so the page boundary
/// is a plain offset. It carries no query or sort identity: callers must retain
/// the same filters and sort, and restart without a cursor when either changes.
/// Wrapping it keeps the on-the-wire contract opaque, like the timeline cursor.
pub fn encode_offset(offset: usize) -> String {
    URL_SAFE_NO_PAD.encode(offset.to_string())
}

/// Decode an opaque search cursor back into an offset. Returns `None` for any
/// malformed input so the handler can map it to a `400`.
pub fn decode_offset(s: &str) -> Option<usize> {
    if s.len() > MAX_ENCODED_LEN {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(s).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    text.parse().ok()
}

/// Decode a search offset cursor, rejecting offsets past `max`. Offset pagination
/// is skip-N work at the index (Tantivy keeps the top `offset + limit` docs), so an
/// attacker-forged cursor for a huge offset is a resource-amplification vector; this
/// is the single place offset-backed cursors opt into an explicit resource cap.
/// Returns `None` for malformed input *or* an in-range-but-too-large offset, both of
/// which the handler maps to a `400`.
pub fn decode_offset_bounded(s: &str, max: usize) -> Option<usize> {
    let offset = decode_offset(s)?;
    (offset <= max).then_some(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let c = TimelineCursor {
            origin_ts: 1_700_000_000_000,
            id: 42,
        };
        let decoded = decode(&encode(c)).expect("decode");
        assert_eq!(decoded, c);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode("not base64!!!").is_none());
        assert!(decode("Zm9vYmFy").is_none()); // "foobar" — no dot
    }

    #[test]
    fn offset_round_trips() {
        assert_eq!(decode_offset(&encode_offset(0)), Some(0));
        assert_eq!(decode_offset(&encode_offset(12_345)), Some(12_345));
    }

    #[test]
    fn offset_rejects_garbage() {
        assert!(decode_offset("not base64!!!").is_none());
        assert!(decode_offset(&URL_SAFE_NO_PAD.encode("-1")).is_none()); // not a usize
    }

    #[test]
    fn offset_rejects_overlong_encoding() {
        // A huge decimal (well-formed base64url, parses as a usize) whose encoded
        // form exceeds the length guard is rejected before decoding.
        let long = URL_SAFE_NO_PAD.encode("9".repeat(100));
        assert!(long.len() > MAX_ENCODED_LEN);
        assert!(decode_offset(&long).is_none());
    }

    #[test]
    fn bounded_offset_rejects_out_of_range() {
        // In range: decodes.
        assert_eq!(
            decode_offset_bounded(&encode_offset(500), 10_000),
            Some(500)
        );
        assert_eq!(
            decode_offset_bounded(&encode_offset(10_000), 10_000),
            Some(10_000),
            "the cap itself is allowed"
        );
        // Past the cap: rejected, even though it's a well-formed offset cursor.
        assert!(decode_offset_bounded(&encode_offset(10_001), 10_000).is_none());
        assert!(decode_offset_bounded(&encode_offset(usize::MAX), 10_000).is_none());
        // Still rejects plain garbage.
        assert!(decode_offset_bounded("not base64!!!", 10_000).is_none());
    }
}
