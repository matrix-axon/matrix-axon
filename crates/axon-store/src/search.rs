//! Store support for the full-text search index (M9). Three concerns, all serving
//! `axon-search`'s convergence contract (ADR 0039):
//!
//! * [`Store::events_for_index`] — a batched, keyset-paginated stream of the
//!   message events worth indexing, each already resolved through the M8
//!   projection (latest edited body, redaction-masked). This is the authoritative
//!   **rebuild source**: a lost or schema-bumped index is reconstructed by
//!   re-scanning it from `id = 0`. It never does a query per event.
//! * The `search_outbox` accessors ([`Store::drain_search_outbox`],
//!   [`Store::prune_search_outbox`]) — the durable change log the indexer drains.
//!   Rows are *appended transactionally* by the event writes themselves (see
//!   `upsert_event`, `update_decrypted_event`, `delete_account_row`), so a change
//!   and its indexing obligation commit atomically.
//! * The `outbox_cursor` accessors ([`Store::search_outbox_cursor`],
//!   [`Store::set_search_outbox_cursor`]) — the indexer's durable high-water mark,
//!   a contiguous complete prefix of applied-and-committed `seq`s.
//!
//! The index itself is derived data keyed by `(account_id, event_id)`, so every
//! read here is safe to replay: a from-scratch rebuild always converges.

use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::events::TIMELINE_SELECT;
use crate::{Store, StoreError};

/// The `search_meta` key whose value is the indexer's durable outbox cursor — the
/// highest `seq` applied to the index and committed.
const OUTBOX_CURSOR_KEY: &str = "outbox_cursor";

/// The `event_id` sentinel in a [`SearchOutboxEntry`] meaning "purge every
/// document for this account" (written when an account row is deleted). A real
/// Matrix event id is never empty, so this is unambiguous.
pub const SEARCH_OUTBOX_PURGE: &str = "";

/// Prefix marking a [`SearchOutboxEntry`] `event_id` as a *room* purge: "delete
/// every document for this account in the given room" (written when a room is
/// purged — see [`Store::purge_room`](crate::Store::purge_room)). The room id
/// follows the prefix. A real Matrix event id starts with `$`, never the unit
/// separator, so this is unambiguous — and distinct from the account-purge
/// sentinel (`""`), which does not start with the prefix.
pub const SEARCH_OUTBOX_ROOM_PURGE_PREFIX: &str = "\u{1f}room\u{1f}";

/// Build the `search_outbox` `event_id` sentinel for a room purge of `room_id`.
pub fn room_purge_sentinel(room_id: &str) -> String {
    format!("{SEARCH_OUTBOX_ROOM_PURGE_PREFIX}{room_id}")
}

/// One message event resolved for indexing: the fields the Tantivy schema needs,
/// with `body` already the resolved (latest-edit, non-redacted) text.
///
/// Distinct from [`crate::TimelineRow`] (the full timeline projection) because
/// the index needs only these six fields and — unlike a per-account timeline
/// read — carries `account_id` explicitly, since [`Store::events_for_index`]
/// streams across all accounts (the index is one combined index with
/// `account_id` as a facet).
#[derive(Debug, Clone)]
pub struct IndexableEvent {
    /// The row's monotonic store id — the keyset cursor for the next batch.
    pub id: i64,
    /// Axon account this event belongs to (the index's `account_id` facet).
    pub account_id: Uuid,
    /// Matrix event ID (the index document key).
    pub event_id: String,
    /// Matrix room ID (the index's `room_id` facet).
    pub room_id: String,
    /// Matrix user ID of the sender (the index's `sender` facet).
    pub sender: String,
    /// `origin_server_ts` in milliseconds (the index's `origin_ts` field).
    pub origin_ts: i64,
    /// The resolved, non-redacted body to index — the latest edit's text when the
    /// event was edited, never `None` (the query filters bodyless rows out).
    pub body: String,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for IndexableEvent {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(IndexableEvent {
            id: row.try_get("id")?,
            account_id: row.try_get("account_id")?,
            event_id: row.try_get("event_id")?,
            room_id: row.try_get("room_id")?,
            sender: row.try_get("sender")?,
            origin_ts: row.try_get("origin_ts")?,
            body: row.try_get("decrypted_body_text")?,
        })
    }
}

impl Store {
    /// One batch of message events to index, ordered by store `id` ascending and
    /// starting strictly after `after_id` (pass `0` for the first batch). Returns
    /// at most `limit` rows; fewer than `limit` means the stream is exhausted.
    ///
    /// Keyset pagination (`id > $1`) rather than `OFFSET` so the bulk build holds
    /// at most one connection at a time and each batch is a bounded indexed scan,
    /// using the maintenance pool and its longer deadline so live sync and API
    /// reads retain their own connections.
    ///
    /// The result mirrors what the live indexing path would derive event-by-event
    /// (`axon_search`'s `indexable`), so a from-scratch rebuild converges with
    /// incremental indexing: each row's `body` is the M8-resolved text (latest
    /// valid edit folded in), and standalone relation events are excluded —
    /// `m.replace` edits (their text is folded into the target row) and
    /// `m.reaction` annotations (no body of their own). The exclusion list here
    /// must stay in lockstep with `is_standalone_relation` in `axon-search`. The
    /// `decrypted_body_text IS NOT NULL` filter is on the *base* row, so it keeps
    /// message events and drops non-text events; a redacted target still passes
    /// that filter but the projection masks its body to `NULL`, so it is excluded
    /// by the post-projection guard — redacted content is never indexed. The
    /// post-projection guard also drops empty resolved bodies (`<> ''`), matching
    /// the live path, so neither side indexes a bodyless document.
    pub async fn events_for_index(
        &self,
        after_id: i64,
        limit: i64,
    ) -> Result<Vec<IndexableEvent>, StoreError> {
        // `TIMELINE_SELECT` is a complete projection that takes an appended
        // `WHERE` (referencing its inner `e` alias) but does not itself expose
        // `account_id`. Wrap it as `proj` and join back on the globally-unique
        // store `id` to recover `account_id`, keeping the M8 edit-collapse /
        // redaction-masking intact without duplicating that SQL here.
        let sql = format!(
            "SELECT proj.id, ev.account_id, proj.event_id, proj.room_id, \
                    proj.sender, proj.origin_ts, proj.decrypted_body_text \
             FROM ( \
                 {} \
                 WHERE e.id > $1 \
                   AND e.decrypted_body_text IS NOT NULL \
                   AND COALESCE(e.relates_to->>'rel_type', '') <> 'm.replace' \
                   AND NOT (COALESCE(e.relates_to->>'rel_type', '') = 'm.annotation' \
                            AND e.event_type = 'm.reaction') \
             ) proj \
             JOIN events ev ON ev.id = proj.id \
             WHERE proj.decrypted_body_text IS NOT NULL \
               AND proj.decrypted_body_text <> '' \
             ORDER BY proj.id ASC \
             LIMIT $2",
            TIMELINE_SELECT.as_str()
        );
        let rows = sqlx_core::query_as::query_as::<Postgres, IndexableEvent>(&sql)
            .bind(after_id)
            .bind(limit)
            .fetch_all(&self.maintenance_pool)
            .await?;
        Ok(rows)
    }

    /// One batch of outbox work items with `seq > after_seq`, in `seq` order, at
    /// most `limit` rows. Fewer than `limit` means the outbox is drained. The
    /// indexer applies each to Tantivy in order, then advances the cursor with
    /// [`set_search_outbox_cursor`](Store::set_search_outbox_cursor) and prunes
    /// with [`prune_search_outbox`](Store::prune_search_outbox).
    pub async fn drain_search_outbox(
        &self,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<SearchOutboxEntry>, StoreError> {
        let rows = sqlx_core::query_as::query_as::<Postgres, SearchOutboxEntry>(
            "SELECT seq, account_id, event_id FROM search_outbox \
             WHERE seq > $1 ORDER BY seq ASC LIMIT $2",
        )
        .bind(after_seq)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// The current highest `seq` in the outbox (`0` if empty). Captured *before* a
    /// corpus seed and then stored as the cursor: the seed (which runs after) holds
    /// everything committed by this point, so the post-seed drain need only apply
    /// later changes.
    pub async fn search_outbox_high_water(&self) -> Result<i64, StoreError> {
        let (hw,): (i64,) = sqlx_core::query_as::query_as::<Postgres, (i64,)>(
            "SELECT COALESCE(MAX(seq), 0) FROM search_outbox",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(hw)
    }

    /// The indexer's durable outbox cursor — the highest `seq` whose effect has
    /// been applied to the index and committed. `0` (never advanced) means "start
    /// from the beginning of the outbox".
    pub async fn search_outbox_cursor(&self) -> Result<i64, StoreError> {
        let row = sqlx_core::query_as::query_as::<Postgres, (String,)>(
            "SELECT value FROM search_meta WHERE key = $1",
        )
        .bind(OUTBOX_CURSOR_KEY)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(v,)| v.parse().ok()).unwrap_or(0))
    }

    /// Advance the durable outbox cursor to `seq`. Called only after the batch
    /// through `seq` has been committed to Tantivy, so the stored value is always a
    /// contiguous complete prefix of applied work.
    pub async fn set_search_outbox_cursor(&self, seq: i64) -> Result<(), StoreError> {
        sqlx_core::query::query(
            "INSERT INTO search_meta (key, value) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(OUTBOX_CURSOR_KEY)
        .bind(seq.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete drained outbox rows (`seq <= through_seq`). Safe because the rebuild
    /// source is `events`, never the outbox — pruning can never remove information
    /// needed to reconstruct the index. Idempotent.
    pub async fn prune_search_outbox(&self, through_seq: i64) -> Result<(), StoreError> {
        // Each committed batch releases its connection and row locks. A crash
        // or timeout leaves an already-applied prefix that the next prune can
        // safely retry, without replaying or losing search work.
        loop {
            let result = sqlx_core::query::query(
                "DELETE FROM search_outbox WHERE seq IN ( \
                     SELECT seq FROM search_outbox WHERE seq <= $1 ORDER BY seq LIMIT 1000 \
                 )",
            )
            .bind(through_seq)
            .execute(&self.maintenance_pool)
            .await?;
            if result.rows_affected() == 0 {
                return Ok(());
            }
        }
    }
}

/// One row of the `search_outbox` change log: a resolved document that may have
/// changed and must be (re)derived, or — when `event_id` is
/// [`SEARCH_OUTBOX_PURGE`] — an account-wide purge.
#[derive(Debug, Clone)]
pub struct SearchOutboxEntry {
    /// Monotonic log position; the indexer's cursor advances over this.
    pub seq: i64,
    /// The affected account.
    pub account_id: Uuid,
    /// The affected Matrix event id, or [`SEARCH_OUTBOX_PURGE`] for a purge.
    pub event_id: String,
}

impl SearchOutboxEntry {
    /// Whether this entry is the account-purge sentinel (delete every document
    /// for the account).
    pub fn is_purge(&self) -> bool {
        self.event_id == SEARCH_OUTBOX_PURGE
    }

    /// If this entry is a *room* purge, the room id to purge (every document for
    /// this account in that room); `None` otherwise. Distinct from
    /// [`is_purge`](Self::is_purge): a room purge names one room, an account purge
    /// removes everything.
    pub fn room_purge(&self) -> Option<&str> {
        self.event_id.strip_prefix(SEARCH_OUTBOX_ROOM_PURGE_PREFIX)
    }
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for SearchOutboxEntry {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(SearchOutboxEntry {
            seq: row.try_get("seq")?,
            account_id: row.try_get("account_id")?,
            event_id: row.try_get("event_id")?,
        })
    }
}
