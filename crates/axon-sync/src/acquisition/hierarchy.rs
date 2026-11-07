//! Hierarchy crawls of joined spaces (ADR 0128 decisions 3, 4, 10, and 11).
//!
//! A crawl is a sequence of [`axon_core::RoomDetailJob::Hierarchy`] jobs, one page each,
//! on the account's acquisition worker. Each job asks the store where the
//! crawl stands, sends one `GET /_matrix/client/v1/rooms/{space}/hierarchy`
//! with `max_depth=1`, and records the page; the worker then queues the next
//! page behind whatever is already waiting.
//!
//! Entries are validated field by field: a field that fails is withheld and
//! named, and only an entry with no valid room id is skipped whole. Logs name
//! counts and categories, never names, topics, aliases, bodies, or tokens.
use std::time::Duration;

use axon_store::{CrawlBegin, CrawlLimits, CrawlStep, CrawlWrite, DiscoveredEntry};
use matrix_sdk::ruma::{OwnedMxcUri, OwnedRoomAliasId, OwnedRoomId, RoomId};
use serde_json::{Map, Value};

use super::CONCLUSIVE_DENIAL_TTL;
use super::{ms, Get, NoAnswer, Ran, Reply, Upstream, Worker};
use super::{FAILURE_BACKOFF_MAX, FAILURE_BACKOFF_MIN, UNSUPPORTED_TTL};

/// Synapse's server-side page maximum; asking for more changes nothing.
pub(crate) const PAGE_LIMIT: i32 = 50;
/// 2,000 children per crawl; a larger space is reported `truncated`.
pub(crate) const MAX_PAGES: i32 = 40;
/// Fifty summaries plus moderate `children_state`; a larger page is asked
/// again at half the page limit.
pub(crate) const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;
/// One restart covers a crawl paused past Synapse's 5-minute session.
pub(crate) const MAX_CURSOR_RESTARTS: i32 = 1;
/// One page can wait on federation for several remote children.
pub(crate) const HIERARCHY_TIMEOUT: Duration = Duration::from_secs(30);
/// Space membership and child names change slowly.
pub(crate) const CRAWL_TTL: Duration = Duration::from_secs(6 * 60 * 60);
/// Display names beyond this are pathological.
pub(crate) const MAX_NAME_BYTES: usize = 1024;
/// Topics are longer than names but are shown as summaries.
pub(crate) const MAX_TOPIC_BYTES: usize = 8 * 1024;
/// Identifiers and enumerations: join rules, room types, encryption
/// algorithms, room versions, aliases. The spec limits aliases to 255 bytes.
const MAX_SHORT_BYTES: usize = 255;
/// The longest `next_batch` kept. Tokens are opaque but short in every
/// homeserver; a longer one is a malformed response, not a cursor to store.
pub(crate) const MAX_TOKEN_BYTES: usize = 1024;
/// The spec's limit on a room id.
const MAX_ROOM_ID_BYTES: usize = 255;

pub(crate) fn limits() -> CrawlLimits {
    CrawlLimits {
        page_limit: PAGE_LIMIT,
        max_pages: MAX_PAGES,
        max_cursor_restarts: MAX_CURSOR_RESTARTS,
        crawl_ttl_ms: ms(CRAWL_TTL),
        min_backoff_ms: ms(FAILURE_BACKOFF_MIN),
        max_backoff_ms: ms(FAILURE_BACKOFF_MAX),
    }
}

/// The summary fields a hierarchy entry or room summary carries, with how
/// each is validated. The names are the spec's and the wire's.
const FIELDS: [(&str, Field); 11] = [
    ("name", Field::Text(MAX_NAME_BYTES)),
    ("topic", Field::Text(MAX_TOPIC_BYTES)),
    ("avatar_url", Field::Mxc),
    ("canonical_alias", Field::Alias),
    ("num_joined_members", Field::Count),
    ("join_rule", Field::Text(MAX_SHORT_BYTES)),
    ("room_type", Field::Text(MAX_SHORT_BYTES)),
    ("world_readable", Field::Bool),
    ("guest_can_join", Field::Bool),
    ("encryption", Field::Text(MAX_SHORT_BYTES)),
    ("room_version", Field::Text(MAX_SHORT_BYTES)),
];

#[derive(Debug, Clone, Copy)]
enum Field {
    /// A string of at most this many bytes.
    Text(usize),
    /// An `mxc://` URI.
    Mxc,
    /// A room alias.
    Alias,
    /// A non-negative integer.
    Count,
    Bool,
}

impl Field {
    fn valid(self, value: &Value) -> bool {
        // PostgreSQL stores no U+0000 in `text` or `jsonb`, so a string that
        // carries one could never commit; withhold it like any invalid field.
        if value.as_str().is_some_and(|text| text.contains('\0')) {
            return false;
        }
        match self {
            Field::Text(max) => value.as_str().is_some_and(|text| text.len() <= max),
            Field::Mxc => value.as_str().is_some_and(|text| {
                text.len() <= MAX_NAME_BYTES && OwnedMxcUri::from(text).is_valid()
            }),
            Field::Alias => value.as_str().is_some_and(|text| {
                text.len() <= MAX_SHORT_BYTES && OwnedRoomAliasId::try_from(text).is_ok()
            }),
            Field::Count => value.as_u64().is_some_and(|n| i64::try_from(n).is_ok()),
            Field::Bool => value.is_boolean(),
        }
    }
}

/// One entry's validated summary, or `None` when it has no valid room id or
/// is not an object. An absent or `null` field is missing, not withheld.
pub(crate) fn parse_entry(entry: &Value) -> Option<DiscoveredEntry> {
    let object = entry.as_object()?;
    let room_id = object.get("room_id")?.as_str()?;
    if room_id.len() > MAX_ROOM_ID_BYTES || OwnedRoomId::try_from(room_id).is_err() {
        return None;
    }
    let mut summary = Map::new();
    let mut withheld = Vec::new();
    for (name, field) in FIELDS {
        match object.get(name) {
            None | Some(Value::Null) => {}
            Some(value) if field.valid(value) => {
                summary.insert(name.to_owned(), value.clone());
            }
            Some(_) => withheld.push(name),
        }
    }
    Some(DiscoveredEntry {
        room_id: room_id.to_owned(),
        summary: Value::Object(summary),
        withheld,
    })
}

/// A `200` hierarchy body as a page, or `None` when its shape is wrong: no
/// `rooms` array, or a `next_batch` that is not a usable token (empty, over
/// [`MAX_TOKEN_BYTES`], or carrying a U+0000 PostgreSQL cannot store). The
/// requested space's own entry is skipped; `children_state` and every other
/// unknown field are ignored.
pub(crate) fn parse_page(body: &[u8], space_id: &str) -> Option<CrawlStep> {
    let body: Value = serde_json::from_slice(body).ok()?;
    let rooms = body.get("rooms")?.as_array()?;
    let next_batch = match body.get("next_batch") {
        None | Some(Value::Null) => None,
        Some(Value::String(token))
            if !token.is_empty() && token.len() <= MAX_TOKEN_BYTES && !token.contains('\0') =>
        {
            Some(token.clone())
        }
        Some(_) => return None,
    };
    let mut entries = Vec::with_capacity(rooms.len());
    let mut unidentified = 0_i32;
    for entry in rooms {
        match parse_entry(entry) {
            Some(entry) if entry.room_id == space_id => {}
            Some(entry) => entries.push(entry),
            None => unidentified = unidentified.saturating_add(1),
        }
    }
    Some(CrawlStep::Page {
        entries,
        unidentified,
        next_batch,
    })
}

fn failure(category: &'static str, retry_after_ms: Option<i64>) -> CrawlStep {
    CrawlStep::Failure {
        category,
        retry_after_ms,
    }
}

/// What the crawl records for `reply`. `resumed` says the request carried a
/// `from` token, so `M_INVALID_PARAM` means the session expired.
pub(crate) fn interpret(reply: Reply, space_id: &str, resumed: bool) -> CrawlStep {
    match reply {
        Reply::Failed(NoAnswer::Timeout) => failure("timeout", None),
        Reply::Failed(NoAnswer::Transport) => failure("transport", None),
        Reply::Failed(NoAnswer::Unauthorized) => failure("unauthorized", None),
        Reply::TooLarge => CrawlStep::TooLarge,
        Reply::Ok(body) => {
            parse_page(&body, space_id).unwrap_or_else(|| failure("invalid_response", None))
        }
        Reply::Status {
            status,
            errcode,
            retry_after_ms,
        } => match (status, errcode.as_deref()) {
            (400, Some("M_INVALID_PARAM")) if resumed => CrawlStep::CursorExpired,
            (401, _) => failure("unauthorized", retry_after_ms),
            (403, _) => CrawlStep::Denied {
                failure: "forbidden",
                ttl_ms: ms(CONCLUSIVE_DENIAL_TTL),
            },
            (404, Some("M_NOT_FOUND")) => CrawlStep::Denied {
                failure: "not_found",
                ttl_ms: ms(CONCLUSIVE_DENIAL_TTL),
            },
            (404, None | Some("M_UNRECOGNIZED")) | (405, _) => CrawlStep::Unsupported {
                ttl_ms: ms(UNSUPPORTED_TTL),
            },
            (429, _) => failure("rate_limited", retry_after_ms),
            (500..=599, _) => failure("server_error", retry_after_ms),
            _ => failure("invalid_response", None),
        },
    }
}

/// A short, allowlisted name for a step, for logs.
fn step_label(step: &CrawlStep) -> &'static str {
    match step {
        CrawlStep::Page { .. } => "page",
        CrawlStep::TooLarge => "too_large",
        CrawlStep::CursorExpired => "cursor_expired",
        CrawlStep::Denied { failure, .. } => failure,
        CrawlStep::Unsupported { .. } => "unsupported",
        CrawlStep::Failure { category, .. } => category,
    }
}

impl<U: Upstream> Worker<U> {
    /// Run one page of `space_id`'s crawl, fenced by the membership `epoch`
    /// the job started in.
    pub(super) async fn run_hierarchy(&mut self, space_id: &RoomId, epoch: i64) -> Ran {
        let account_id = self.account_id;
        let mut ran = Ran::default();
        let limits = limits();
        let position = match self
            .store
            .begin_space_crawl(account_id, space_id.as_str(), epoch, limits)
            .await
        {
            Ok(CrawlBegin::Page(position)) => position,
            Ok(CrawlBegin::NotDue) => return ran,
            Ok(CrawlBegin::Fenced) => {
                tracing::debug!(target: "axon_sync::acquisition", %account_id, room_id = %space_id, kind = "hierarchy", outcome = "not_joined", "skipped space hierarchy page");
                return ran;
            }
            Err(error) => {
                tracing::warn!(target: "axon_sync::acquisition", %account_id, room_id = %space_id, kind = "hierarchy", reason = error.diagnostic_reason(), "space hierarchy crawl could not read its state");
                ran.local_failure = true;
                return ran;
            }
        };
        let limit = position.page_limit.to_string();
        let mut query = vec![
            ("max_depth", "1"),
            ("suggested_only", "false"),
            ("limit", limit.as_str()),
        ];
        if let Some(from) = position.from.as_deref() {
            query.push(("from", from));
        }
        let segments = ["rooms", space_id.as_str(), "hierarchy"];
        let (reply, elapsed_ms) = self
            .paced(Get {
                version: "v1",
                segments: &segments,
                query: &query,
                body_limit: Some(MAX_PAGE_BYTES),
                timeout: HIERARCHY_TIMEOUT,
            })
            .await;
        let step = interpret(reply, space_id.as_str(), position.from.is_some());
        let label = step_label(&step);
        let (returned, withheld_fields, unidentified) = match &step {
            CrawlStep::Page {
                entries,
                unidentified,
                ..
            } => {
                let mut withheld: Vec<&str> = entries
                    .iter()
                    .flat_map(|entry| entry.withheld.iter().copied())
                    .collect();
                withheld.sort_unstable();
                withheld.dedup();
                (entries.len(), withheld, *unidentified)
            }
            _ => (0, Vec::new(), 0),
        };
        let page = position.pages + 1;
        let write = match self
            .store
            .record_space_crawl(
                account_id,
                space_id.as_str(),
                epoch,
                position.generation,
                limits,
                &step,
            )
            .await
        {
            Ok(write) => write,
            Err(error) => {
                tracing::warn!(target: "axon_sync::acquisition", %account_id, room_id = %space_id, kind = "hierarchy", outcome = label, page, reason = error.diagnostic_reason(), "space hierarchy crawl could not record its page");
                ran.local_failure = true;
                return ran;
            }
        };
        log_page(
            account_id,
            space_id,
            label,
            elapsed_ms,
            page,
            position.page_limit,
            position.restarts,
            returned,
            unidentified,
            &withheld_fields,
            &write,
        );
        if write.fenced {
            return ran;
        }
        ran.changed = write.changed;
        ran.conclusive = write.conclusive;
        ran.next_page = write.next_page;
        ran
    }
}

#[allow(clippy::too_many_arguments)]
fn log_page(
    account_id: uuid::Uuid,
    space_id: &RoomId,
    label: &'static str,
    elapsed_ms: u64,
    page: i32,
    page_limit: i32,
    restarts: i32,
    returned: usize,
    unidentified: i32,
    withheld: &[&str],
    write: &CrawlWrite,
) {
    let kind = "hierarchy";
    let outcome = if write.fenced {
        "fenced"
    } else {
        write.outcome
    };
    let failure = write.failure.unwrap_or(label);
    tracing::debug!(
        target: "axon_sync::acquisition",
        %account_id,
        room_id = %space_id,
        kind,
        step = label,
        outcome,
        elapsed_ms,
        page,
        page_limit,
        next_page_limit = write.page_limit,
        returned,
        rooms_cached = write.cached,
        unlinked = write.unlinked,
        joined = write.joined,
        unidentified,
        ?withheld,
        omitted = write.omitted,
        orphans = write.orphans,
        omission_skipped = write.omission_skipped,
        restarts,
        restarted = write.restarted,
        "space hierarchy page"
    );
    if write.fenced {
        return;
    }
    match (write.outcome, write.failure) {
        (_, Some("pagination_loop" | "too_large")) => {
            tracing::warn!(target: "axon_sync::acquisition", %account_id, room_id = %space_id, kind, outcome, failure, page, page_limit, "space hierarchy crawl stopped early");
        }
        // One failure is routine; a second in a row is worth a warning.
        ("failed", Some(_))
            if write.failures > 1 && !matches!(failure, "forbidden" | "not_found") =>
        {
            tracing::warn!(target: "axon_sync::acquisition", %account_id, room_id = %space_id, kind, outcome, failure, page, elapsed_ms, "space hierarchy crawl failed; backing off");
        }
        _ => {}
    }
    if write.next_page && write.page_limit < page_limit {
        tracing::info!(target: "axon_sync::acquisition", %account_id, room_id = %space_id, kind, page, page_limit, next_page_limit = write.page_limit, "space hierarchy page too large; asking again at a smaller limit");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SPACE: &str = "!space:localhost";

    fn page(body: Value) -> CrawlStep {
        interpret(Reply::Ok(serde_json::to_vec(&body).unwrap()), SPACE, false)
    }

    fn status(status: u16, errcode: Option<&str>) -> Reply {
        Reply::Status {
            status,
            errcode: errcode.map(str::to_owned),
            retry_after_ms: Some(7_000),
        }
    }

    #[test]
    fn valid_fields_are_kept_and_invalid_ones_withheld() {
        let entry = parse_entry(&json!({
            "room_id": "!child:localhost",
            "name": "General",
            "topic": "x".repeat(MAX_TOPIC_BYTES + 1),
            "avatar_url": "https://example.org/a.png",
            "canonical_alias": "#general:localhost",
            "num_joined_members": 42,
            "join_rule": "public",
            "room_type": null,
            "world_readable": "yes",
            "guest_can_join": false,
            "encryption": "m.megolm.v1.aes-sha2",
            "room_version": "12",
            "children_state": [{"type": "m.space.child"}],
            "membership": "join",
        }))
        .unwrap();
        assert_eq!(entry.room_id, "!child:localhost");
        assert_eq!(
            entry.summary,
            json!({
                "name": "General",
                "canonical_alias": "#general:localhost",
                "num_joined_members": 42,
                "join_rule": "public",
                "guest_can_join": false,
                "encryption": "m.megolm.v1.aes-sha2",
                "room_version": "12",
            }),
            "children_state, membership, and unknown fields are never kept"
        );
        assert_eq!(entry.withheld, ["topic", "avatar_url", "world_readable"]);
    }

    #[test]
    fn each_field_has_its_own_rule() {
        let withheld = |field: &str, value: Value| {
            let mut entry = json!({"room_id": "!c:localhost"});
            entry[field] = value;
            parse_entry(&entry).unwrap().withheld
        };
        let none: Vec<&str> = Vec::new();
        assert_eq!(withheld("name", json!("n".repeat(MAX_NAME_BYTES))), none);
        assert_eq!(
            withheld("name", json!("n".repeat(MAX_NAME_BYTES + 1))),
            ["name"]
        );
        assert_eq!(withheld("name", json!(7)), ["name"]);
        assert_eq!(withheld("avatar_url", json!("mxc://localhost/abc")), none);
        assert_eq!(withheld("avatar_url", json!("mxc://")), ["avatar_url"]);
        assert_eq!(
            withheld("canonical_alias", json!("not-an-alias")),
            ["canonical_alias"]
        );
        assert_eq!(withheld("num_joined_members", json!(0)), none);
        assert_eq!(
            withheld("num_joined_members", json!(-1)),
            ["num_joined_members"]
        );
        assert_eq!(
            withheld("num_joined_members", json!(1.5)),
            ["num_joined_members"]
        );
        assert_eq!(
            withheld("num_joined_members", json!(u64::MAX)),
            ["num_joined_members"]
        );
        assert_eq!(
            withheld("join_rule", json!("j".repeat(MAX_SHORT_BYTES + 1))),
            ["join_rule"]
        );
        assert_eq!(withheld("guest_can_join", json!(1)), ["guest_can_join"]);
        // PostgreSQL cannot store U+0000, in any string field.
        for field in [
            "name",
            "topic",
            "avatar_url",
            "canonical_alias",
            "join_rule",
            "room_type",
            "encryption",
            "room_version",
        ] {
            assert_eq!(withheld(field, json!("a\u{0}b")), [field], "{field}");
        }
    }

    #[test]
    fn a_missing_encryption_field_is_missing_not_unencrypted() {
        let entry = parse_entry(&json!({"room_id": "!c:localhost"})).unwrap();
        assert_eq!(entry.summary, json!({}));
        assert!(entry.withheld.is_empty());
    }

    #[test]
    fn entries_without_a_room_id_are_unidentified_and_the_space_is_skipped() {
        let step = page(json!({
            "rooms": [
                {"room_id": SPACE, "name": "The space itself"},
                {"room_id": "!a:localhost"},
                {"room_id": "not a room id"},
                {"room_id": format!("!{}:localhost", "x".repeat(300))},
                {"name": "no id"},
                "not an object",
            ],
            "next_batch": "t1",
        }));
        let CrawlStep::Page {
            entries,
            unidentified,
            next_batch,
        } = step
        else {
            panic!("{step:?}");
        };
        let ids: Vec<&str> = entries.iter().map(|e| e.room_id.as_str()).collect();
        assert_eq!(ids, ["!a:localhost"]);
        assert_eq!(unidentified, 4);
        assert_eq!(next_batch.as_deref(), Some("t1"));
    }

    #[test]
    fn a_malformed_page_is_an_invalid_response() {
        let invalid = |body: Value| {
            assert_eq!(
                page(body.clone()),
                failure("invalid_response", None),
                "{body}"
            );
        };
        invalid(json!({}));
        invalid(json!({"rooms": {}}));
        invalid(json!({"rooms": [], "next_batch": 7}));
        invalid(json!({"rooms": [], "next_batch": ""}));
        invalid(json!({"rooms": [], "next_batch": "t".repeat(MAX_TOKEN_BYTES + 1)}));
        invalid(json!({"rooms": [], "next_batch": "t\u{0}"}));
        assert_eq!(
            interpret(Reply::Ok(b"not json".to_vec()), SPACE, false),
            failure("invalid_response", None)
        );
        assert_eq!(
            page(json!({"rooms": [], "next_batch": null})),
            CrawlStep::Page {
                entries: Vec::new(),
                unidentified: 0,
                next_batch: None
            }
        );
    }

    #[test]
    fn replies_map_to_the_documented_steps() {
        assert_eq!(
            interpret(status(400, Some("M_INVALID_PARAM")), SPACE, true),
            CrawlStep::CursorExpired
        );
        assert_eq!(
            interpret(status(400, Some("M_INVALID_PARAM")), SPACE, false),
            failure("invalid_response", None),
            "only a resumed page can have an expired cursor"
        );
        assert_eq!(
            interpret(status(403, Some("M_FORBIDDEN")), SPACE, false),
            CrawlStep::Denied {
                failure: "forbidden",
                ttl_ms: ms(CONCLUSIVE_DENIAL_TTL)
            }
        );
        assert_eq!(
            interpret(status(404, Some("M_NOT_FOUND")), SPACE, false),
            CrawlStep::Denied {
                failure: "not_found",
                ttl_ms: ms(CONCLUSIVE_DENIAL_TTL)
            }
        );
        for reply in [
            status(404, Some("M_UNRECOGNIZED")),
            status(404, None),
            status(405, None),
        ] {
            assert_eq!(
                interpret(reply, SPACE, false),
                CrawlStep::Unsupported {
                    ttl_ms: ms(UNSUPPORTED_TTL)
                }
            );
        }
        assert_eq!(
            interpret(status(429, None), SPACE, true),
            failure("rate_limited", Some(7_000))
        );
        assert_eq!(
            interpret(status(502, None), SPACE, true),
            failure("server_error", Some(7_000))
        );
        assert_eq!(
            interpret(status(401, None), SPACE, true),
            failure("unauthorized", Some(7_000))
        );
        assert_eq!(interpret(Reply::TooLarge, SPACE, true), CrawlStep::TooLarge);
        assert_eq!(
            interpret(Reply::Failed(NoAnswer::Timeout), SPACE, true),
            failure("timeout", None)
        );
    }

    #[test]
    fn the_limits_match_the_adr() {
        assert_eq!(PAGE_LIMIT, 50);
        assert_eq!(MAX_PAGES, 40);
        assert_eq!(MAX_PAGE_BYTES, 2 * 1024 * 1024);
        assert_eq!(MAX_CURSOR_RESTARTS, 1);
        assert_eq!(HIERARCHY_TIMEOUT, Duration::from_secs(30));
        assert_eq!(CRAWL_TTL, Duration::from_secs(6 * 3600));
        assert_eq!(MAX_NAME_BYTES, 1024);
        assert_eq!(MAX_TOPIC_BYTES, 8 * 1024);
        // Each halving of a page limit of 50 down to 1: at most six retries.
        let mut limit = PAGE_LIMIT;
        let mut retries = 0;
        while limit > 1 {
            limit /= 2;
            retries += 1;
        }
        assert_eq!(retries, 5, "50, 25, 12, 6, 3, 1, then too_large");
    }
}
