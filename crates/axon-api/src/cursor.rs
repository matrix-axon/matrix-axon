//! Opaque pagination cursors: the timeline's, search's, and the space
//! hierarchy's, and the clock cursors of incremental reads (ADR 0131, ADR 0134).
//!
//! The store's [`TimelineCursor`] is a `(origin_ts, id)` sort key with public
//! fields. We don't expose those on the wire — instead a cursor is the
//! base64url (no padding) of `"{origin_ts}.{id}"`, so the on-the-wire contract
//! stays fixed even if the internal sort key changes.
//!
//! `.` is an unambiguous separator here: both fields are `i64`, whose decimal
//! form is ASCII digits with at most a leading `-` — never a `.` — so
//! `split_once('.')` always cleaves the two fields cleanly (negatives included).

use axon_store::{SpaceHierarchyCursor, TimelineCursor};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Upper bound on the encoded length of any cursor we'll even attempt to decode.
/// Every cursor this module emits is the base64url of a short decimal string, so
/// anything materially longer is malformed (or hostile) — reject it before we
/// spend work base64-decoding it. Universal hygiene applied by every decoder here.
const MAX_ENCODED_LEN: usize = 64;

/// Encode a read's database clock as an incremental read's opaque cursor
/// (ADR 0131, ADR 0134): microseconds since the epoch. Clients never parse it,
/// so the format can change.
pub fn encode_clock(as_of: DateTime<Utc>) -> String {
    as_of.timestamp_micros().to_string()
}

/// Decode a clock cursor; `None` for anything [`encode_clock`] could not have
/// written, including non-canonical integers such as `+123` or `00123`.
pub fn decode_clock(cursor: &str) -> Option<DateTime<Utc>> {
    if cursor.len() > MAX_ENCODED_LEN {
        return None;
    }
    cursor
        .parse::<i64>()
        .ok()
        .filter(|micros| *micros >= 0)
        .and_then(DateTime::from_timestamp_micros)
        .filter(|as_of| encode_clock(*as_of) == cursor)
}

/// Encode a room-list cursor (ADR 0134): the read's clock, bound to the
/// `account_id` scope it was read in, so a cursor passed back in another scope
/// is recognised and answered in full instead of as a delta that would miss
/// that scope's older rooms. `{micros}` for all accounts,
/// `{micros}.{account_id}` (simple form) for one.
pub fn encode_scoped_clock(as_of: DateTime<Utc>, scope: Option<Uuid>) -> String {
    match scope {
        None => encode_clock(as_of),
        Some(account_id) => format!("{}.{}", encode_clock(as_of), account_id.simple()),
    }
}

/// Decode a room-list cursor into its clock and scope; `None` for anything
/// [`encode_scoped_clock`] could not have written.
pub fn decode_scoped_clock(cursor: &str) -> Option<(DateTime<Utc>, Option<Uuid>)> {
    if cursor.len() > MAX_ENCODED_LEN {
        return None;
    }
    match cursor.split_once('.') {
        None => Some((decode_clock(cursor)?, None)),
        Some((clock, scope)) => {
            let account_id = Uuid::try_parse(scope).ok()?;
            // Only the simple form this module writes, so one cursor has one
            // spelling.
            (account_id.simple().to_string() == scope)
                .then_some((decode_clock(clock)?, Some(account_id)))
        }
    }
}

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

/// Upper bound on an encoded hierarchy cursor. Unlike the numeric cursors it
/// carries strings: a normalized `order` (at most 50 printable ASCII
/// characters) and a child room id (at most 255 bytes, which the store
/// enforces on the links it returns). Even with every room-id byte
/// JSON-escaped, an honest cursor stays well under this.
const MAX_HIERARCHY_ENCODED_LEN: usize = 4096;

/// MSC1772's limit on a valid `order`: at most 50 printable ASCII characters.
/// The store treats any other `order` as absent, so it never issues one.
const MAX_HIERARCHY_ORDER_LEN: usize = 50;

/// The store pages only children whose room id fits Matrix's 255-byte limit.
const MAX_HIERARCHY_ROOM_ID_BYTES: usize = 255;

/// Wire form of a [`SpaceHierarchyCursor`]. `s` binds the cursor to the
/// account and space it was issued for, so a cursor from another space is
/// rejected instead of silently paging that space from an unrelated key.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HierarchyCursorWire {
    s: String,
    o: Option<String>,
    t: i64,
    r: String,
}

/// A short, stable tag for (account, space). Not a secret: it only tells a
/// cursor that belongs here from one that does not.
fn hierarchy_scope(account_id: Uuid, space_room_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(account_id.as_bytes());
    hasher.update(space_room_id.as_bytes());
    let digest = hasher.finalize();
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// Encode a hierarchy keyset position for `space_room_id` into an opaque
/// cursor string.
pub fn encode_hierarchy(
    account_id: Uuid,
    space_room_id: &str,
    cursor: &SpaceHierarchyCursor,
) -> String {
    let wire = HierarchyCursorWire {
        s: hierarchy_scope(account_id, space_room_id),
        o: cursor.order.clone(),
        t: cursor.origin_ts,
        r: cursor.room_id.clone(),
    };
    // Serializing a struct of strings and an integer cannot fail.
    let json = serde_json::to_vec(&wire).expect("hierarchy cursor serializes");
    URL_SAFE_NO_PAD.encode(json)
}

/// Decode a hierarchy cursor issued for `space_room_id`. Returns `None` for
/// malformed input or a cursor issued for another account or space, so the
/// handler can map either to a `400`.
pub fn decode_hierarchy(
    account_id: Uuid,
    space_room_id: &str,
    s: &str,
) -> Option<SpaceHierarchyCursor> {
    if s.len() > MAX_HIERARCHY_ENCODED_LEN {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(s).ok()?;
    let wire: HierarchyCursorWire = serde_json::from_slice(&bytes).ok()?;
    // The scope tag is not a secret, so a client can mint a cursor for its own
    // space. Accept only positions within the bounds the store issues: an
    // `order` that normalization keeps and a room id within 255 bytes with no
    // NUL. A minted position inside those bounds is still accepted, and is
    // harmless: the walk resumes after that key. Anything outside them (a NUL
    // byte, which PostgreSQL refuses in a text parameter, included) stays a
    // 400 here instead of becoming a 500 from the query.
    let order_ok = wire.o.as_deref().is_none_or(|o| {
        o.len() <= MAX_HIERARCHY_ORDER_LEN && o.bytes().all(|b| (0x20..=0x7e).contains(&b))
    });
    let room_ok = wire.r.len() <= MAX_HIERARCHY_ROOM_ID_BYTES && !wire.r.contains('\0');
    (order_ok && room_ok && wire.s == hierarchy_scope(account_id, space_room_id)).then_some(
        SpaceHierarchyCursor {
            order: wire.o,
            origin_ts: wire.t,
            room_id: wire.r,
        },
    )
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

    fn hierarchy_cursor(order: Option<&str>) -> SpaceHierarchyCursor {
        SpaceHierarchyCursor {
            order: order.map(str::to_owned),
            origin_ts: 1_700_000_000_000,
            room_id: "!child:example.org".to_owned(),
        }
    }

    #[test]
    fn hierarchy_round_trips_both_order_groups() {
        let account = Uuid::new_v4();
        for order in [Some("a"), Some(""), None] {
            let c = hierarchy_cursor(order);
            let encoded = encode_hierarchy(account, "!space:example.org", &c);
            assert_eq!(
                decode_hierarchy(account, "!space:example.org", &encoded),
                Some(c)
            );
        }
    }

    #[test]
    fn hierarchy_rejects_a_cursor_from_another_space_or_account() {
        let account = Uuid::new_v4();
        let encoded = encode_hierarchy(account, "!space:example.org", &hierarchy_cursor(None));
        assert!(decode_hierarchy(account, "!other:example.org", &encoded).is_none());
        assert!(decode_hierarchy(Uuid::new_v4(), "!space:example.org", &encoded).is_none());
    }

    #[test]
    fn hierarchy_rejects_garbage_and_overlong_input() {
        let account = Uuid::new_v4();
        assert!(decode_hierarchy(account, "!s:x", "not base64!!!").is_none());
        assert!(decode_hierarchy(account, "!s:x", &URL_SAFE_NO_PAD.encode("{}")).is_none());
        // A timeline cursor is not a hierarchy cursor.
        let timeline = encode(TimelineCursor {
            origin_ts: 1,
            id: 2,
        });
        assert!(decode_hierarchy(account, "!s:x", &timeline).is_none());
        let long = "A".repeat(MAX_HIERARCHY_ENCODED_LEN + 1);
        assert!(decode_hierarchy(account, "!s:x", &long).is_none());
    }

    #[test]
    fn hierarchy_cursor_at_the_store_bounds_fits_the_length_guard() {
        // The longest honest cursor: a 50-character order and a 255-byte room
        // id made entirely of characters JSON must escape.
        let c = SpaceHierarchyCursor {
            order: Some("~".repeat(50)),
            origin_ts: i64::MIN,
            room_id: "\u{1}".repeat(255),
        };
        let encoded = encode_hierarchy(Uuid::new_v4(), "!s:x", &c);
        assert!(encoded.len() <= MAX_HIERARCHY_ENCODED_LEN);
    }

    #[test]
    fn hierarchy_accepts_only_positions_the_store_can_issue() {
        let account = Uuid::new_v4();
        let decodes = |order: Option<String>, room_id: String| {
            let c = SpaceHierarchyCursor {
                order,
                origin_ts: 1,
                room_id,
            };
            decode_hierarchy(account, "!s:x", &encode_hierarchy(account, "!s:x", &c)).is_some()
        };
        // The bounds themselves are honest positions.
        assert!(decodes(Some(" ~".repeat(25)), "x".repeat(255)));
        assert!(decodes(Some(String::new()), String::new()));
        // Orders the store normalizes to absent, and room ids it never pages.
        assert!(!decodes(Some("x".repeat(51)), "!c:x".to_owned()));
        assert!(!decodes(Some("\u{e9}".to_owned()), "!c:x".to_owned()));
        assert!(!decodes(Some("a\0".to_owned()), "!c:x".to_owned()));
        assert!(!decodes(Some("\t".to_owned()), "!c:x".to_owned()));
        assert!(!decodes(None, "x".repeat(256)));
        assert!(!decodes(None, "!a\0b:x".to_owned()));
    }

    #[test]
    fn scoped_clock_cursors_round_trip_and_keep_their_scope() {
        let as_of = DateTime::from_timestamp_micros(1_762_812_345_678_901).unwrap();
        let account = Uuid::new_v4();
        assert_eq!(
            decode_scoped_clock(&encode_scoped_clock(as_of, None)),
            Some((as_of, None))
        );
        assert_eq!(
            decode_scoped_clock(&encode_scoped_clock(as_of, Some(account))),
            Some((as_of, Some(account)))
        );
        // An unscoped cursor is the plain clock cursor.
        assert_eq!(encode_scoped_clock(as_of, None), encode_clock(as_of));
    }

    #[test]
    fn scoped_clock_cursors_have_one_spelling() {
        let as_of = DateTime::from_timestamp_micros(1_762_812_345_678_901).unwrap();
        let account = Uuid::new_v4();
        let clock = encode_clock(as_of);
        for bad in [
            format!("{clock}.{}", account.hyphenated()),
            format!("{clock}.{}", account.simple().to_string().to_uppercase()),
            format!("{clock}."),
            format!(".{}", account.simple()),
            format!("0{clock}.{}", account.simple()),
            format!("{clock}.{}.{}", account.simple(), account.simple()),
        ] {
            assert_eq!(decode_scoped_clock(&bad), None, "{bad}");
        }
    }

    #[test]
    fn clock_decoders_refuse_overlong_input() {
        let long = "1".repeat(MAX_ENCODED_LEN + 1);
        assert_eq!(decode_clock(&long), None);
        assert_eq!(decode_scoped_clock(&long), None);
    }
}
