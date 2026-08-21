//! Room summaries: the cross-account room list that backs `GET /v1/rooms`.
//!
//! Activity (the sort key) and the latest event id live in the
//! `room_summaries` table (ADR 0095), maintained incrementally as events are
//! stored. Display fields (name, topic, avatar, canonical alias, room type) are
//! still point lookups into the resolved [`room_state`](crate::state)
//! projection. A room joined by two accounts yields two summaries, one per
//! `account_id` — the natural identity here is `(account_id, room_id)`.

use sqlx_core::row::Row;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::{Store, StoreError};

/// Incremental `room_summaries` write (ADR 0095). Prepended with a
/// `WITH ins AS ( <event write> RETURNING account_id, room_id, event_id,
/// origin_ts, id, decrypted_body_text )`, it upserts one row per inserted or
/// newly-decrypted event.
///
/// The `ON CONFLICT` predicate is the activity contract `list_rooms` used to
/// recompute from `events` on every request:
///
/// - a content-bearing incoming event (`decrypted_body_text IS NOT NULL`)
///   replaces a non-content marker unconditionally — even if it is older —
///   matching `COALESCE(content MAX, any MAX)`;
/// - two markers of the same kind compare `(origin_ts, id)` and keep the
///   newer;
/// - a non-content event never displaces a content marker.
///
/// A leading `ON CONFLICT DO NOTHING` / `content IS NULL` no-op leaves `ins`
/// empty, so this writes nothing — same atomicity story as the search-outbox
/// fan-out.
pub(crate) const ROOM_SUMMARY_TOUCH_TAIL: &str = "\
    INSERT INTO room_summaries ( \
        account_id, room_id, last_activity_ts, last_event_id, \
        last_event_row_id, last_activity_is_content \
    ) \
    SELECT account_id, room_id, origin_ts, event_id, id, \
           decrypted_body_text IS NOT NULL \
    FROM ins \
    ON CONFLICT (account_id, room_id) DO UPDATE SET \
        last_activity_ts = EXCLUDED.last_activity_ts, \
        last_event_id = EXCLUDED.last_event_id, \
        last_event_row_id = EXCLUDED.last_event_row_id, \
        last_activity_is_content = EXCLUDED.last_activity_is_content \
    WHERE (EXCLUDED.last_activity_is_content \
           AND NOT room_summaries.last_activity_is_content) \
       OR ( \
            EXCLUDED.last_activity_is_content \
                = room_summaries.last_activity_is_content \
            AND (EXCLUDED.last_activity_ts, EXCLUDED.last_event_row_id) \
                > (room_summaries.last_activity_ts, \
                   room_summaries.last_event_row_id) \
          ) \
    RETURNING account_id, room_id, (xmax = 0) AS inserted";

/// One room as it appears in the cross-account room list: identity, the four
/// common display fields resolved from current room state, and the
/// most-recent-activity sort key.
#[derive(Debug, Clone)]
pub struct RoomSummary {
    /// Axon account this room belongs to.
    pub account_id: Uuid,
    /// Matrix user ID for this Axon account.
    pub account_user_id: String,
    /// Matrix room ID.
    pub room_id: String,
    /// `m.room.name` → `content.name`, if set.
    pub name: Option<String>,
    /// `m.room.topic` → `content.topic`, if set.
    pub topic: Option<String>,
    /// `m.room.avatar` → `content.url` (an `mxc://` URI), if set. Note the state
    /// event type is `m.room.avatar`, and the field inside it is `url`.
    pub avatar_url: Option<String>,
    /// `m.room.canonical_alias` → `content.alias`, if set.
    pub canonical_alias: Option<String>,
    /// `m.room.create` → `content.type`, if set (for example `m.space`).
    pub room_type: Option<String>,
    /// `origin_ts` of the room's latest content-bearing event (a non-null
    /// `decrypted_body_text` — see [`Store::list_rooms`]), in milliseconds —
    /// the sort key. Falls back to the latest event of any type when the room
    /// has no content-bearing event.
    pub last_activity_ts: i64,
    /// The `event_id` at `last_activity_ts` (latest by `(origin_ts, id)`),
    /// among the same content-bearing events.
    pub last_event_id: Option<String>,
    /// SDK-derived unread notification count (issue #313, ADR 0070) — a
    /// cached read of matrix-sdk's client-side
    /// `Room::num_unread_notifications()` counter. `0` until the sync engine's
    /// watcher has written a value for this room (see
    /// [`Store::upsert_room_unread_counts`]).
    pub notification_count: i64,
    /// SDK-derived highlight count (issue #313, ADR 0070), from matrix-sdk's
    /// `Room::num_unread_mentions()` counter.
    pub highlight_count: i64,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for RoomSummary {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(RoomSummary {
            account_id: row.try_get("account_id")?,
            account_user_id: row.try_get("account_user_id")?,
            room_id: row.try_get("room_id")?,
            name: row.try_get("name")?,
            topic: row.try_get("topic")?,
            avatar_url: row.try_get("avatar_url")?,
            canonical_alias: row.try_get("canonical_alias")?,
            room_type: row.try_get("room_type")?,
            last_activity_ts: row.try_get("last_activity_ts")?,
            last_event_id: row.try_get("last_event_id")?,
            notification_count: row.try_get("notification_count")?,
            highlight_count: row.try_get("highlight_count")?,
        })
    }
}

impl Store {
    /// List rooms across accounts, most-recent-activity first. `account_id =
    /// Some(id)` narrows to one account; `None` returns every account's rooms
    /// (each tagged with its own `account_id`).
    ///
    /// One query: `room_summaries` (ADR 0095) supplies the activity timestamp,
    /// latest event id, and the list-driving display / visibility fields —
    /// one row per room, maintained incrementally by
    /// [`upsert_event`](Self::upsert_event),
    /// [`update_decrypted_event`](Self::update_decrypted_event), and
    /// [`upsert_room_state`](Self::upsert_room_state) rather than recomputed
    /// from `events` / `room_state` on every request. A single join to
    /// `room_unread_counts` (issue #313, ADR 0070) supplies the unread
    /// counts. `COALESCE(..., 0)` covers a room with no unread-counts row yet
    /// (between account creation and the sync engine's first sweep), so the
    /// fields read as `0` rather than `NULL`.
    ///
    /// The activity timestamp and latest event id each prefer content-bearing
    /// events — `decrypted_body_text IS NOT NULL`, the same "does this event
    /// have a displayable body" signal `axon-search` indexing and
    /// `room_timeline` already key off of — falling back to the newest event
    /// of any type only when a room has no content-bearing event at all. That
    /// keeps membership changes, profile/avatar/name/topic state, redactions,
    /// bare reactions, and still-undecrypted `m.room.encrypted` rows from
    /// bumping a room's position once it has real messages, while a
    /// just-joined or message-less room still appears in the list (sorted by
    /// whatever non-content event it does have) instead of vanishing. A
    /// decrypted message keeps the cleartext event type once matrix-rust-sdk
    /// decrypts it, so E2EE rooms are unaffected in the normal case. The
    /// comparison lives in [`ROOM_SUMMARY_TOUCH_TAIL`], not in this read.
    ///
    /// Rooms the local user has left or been banned from are excluded via
    /// `hidden_left`, maintained when the local user's `m.room.member` state
    /// is upserted (ADR 0037). The predicate hides only on a definitive
    /// leave/ban signal — a room with no membership row for the local user
    /// still appears (`hidden_left = false`), so missing membership data never
    /// hides a joined room.
    ///
    /// Tombstoned rooms (upgraded via `m.room.tombstone`) are excluded via
    /// `hidden_tombstoned`: the old room is superseded by its replacement and
    /// should not appear alongside the new room in the list.
    pub async fn list_rooms(
        &self,
        account_id: Option<Uuid>,
    ) -> Result<Vec<RoomSummary>, StoreError> {
        let rows = sqlx_core::query_as::query_as::<Postgres, RoomSummary>(
            "SELECT a.account_id, ac.user_id AS account_user_id, a.room_id, \
                    a.last_activity_ts, a.last_event_id, \
                    a.name, a.topic, a.avatar_url, a.canonical_alias, a.room_type, \
                    COALESCE(ruc.notification_count, 0) AS notification_count, \
                    COALESCE(ruc.highlight_count, 0) AS highlight_count \
             FROM room_summaries a \
             JOIN accounts ac ON ac.account_id = a.account_id AND ac.state = 'active' \
             LEFT JOIN room_unread_counts ruc \
               ON ruc.account_id = a.account_id AND ruc.room_id = a.room_id \
             WHERE ($1::uuid IS NULL OR a.account_id = $1) \
               AND NOT a.hidden_left \
               AND NOT a.hidden_tombstoned \
             ORDER BY a.last_activity_ts DESC, a.room_id",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Rebuild `room_summaries` for one account from `events`.
    ///
    /// Deletes the account's existing summary rows and re-inserts them with
    /// the same `DISTINCT ON` shape the migration used, so a drifted row
    /// (or a forgotten write-path hook) can be repaired without restarting.
    /// Display and visibility are refreshed from `room_state` in the **same
    /// transaction** (issue #211) so a concurrent `list_rooms` cannot observe
    /// rebuilt rows with the column defaults. Not on the request path —
    /// `list_rooms` reads whatever is already persisted. Returns the number of
    /// summary rows written.
    pub async fn rebuild_room_summaries(&self, account_id: Uuid) -> Result<u64, StoreError> {
        let mut tx: Transaction<'_, Postgres> = self.pool.begin().await?;
        sqlx_core::query::query("DELETE FROM room_summaries WHERE account_id = $1")
            .bind(account_id)
            .execute(&mut *tx)
            .await?;
        let content = sqlx_core::query::query(
            "INSERT INTO room_summaries ( \
                 account_id, room_id, last_activity_ts, last_event_id, \
                 last_event_row_id, last_activity_is_content \
             ) \
             SELECT DISTINCT ON (account_id, room_id) \
                 account_id, room_id, origin_ts, event_id, id, TRUE \
             FROM events \
             WHERE account_id = $1 AND decrypted_body_text IS NOT NULL \
             ORDER BY account_id, room_id, origin_ts DESC, id DESC",
        )
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
        let fallback = sqlx_core::query::query(
            "INSERT INTO room_summaries ( \
                 account_id, room_id, last_activity_ts, last_event_id, \
                 last_event_row_id, last_activity_is_content \
             ) \
             SELECT DISTINCT ON (e.account_id, e.room_id) \
                 e.account_id, e.room_id, e.origin_ts, e.event_id, e.id, FALSE \
             FROM events e \
             WHERE e.account_id = $1 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM room_summaries s \
                   WHERE s.account_id = e.account_id AND s.room_id = e.room_id \
               ) \
             ORDER BY e.account_id, e.room_id, e.origin_ts DESC, e.id DESC",
        )
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
        // Same plpgsql function the live write path uses, so display /
        // visibility cannot drift from a second SQL copy. Inside this
        // transaction so a concurrent list_rooms cannot observe rebuilt
        // rows with the column defaults (hidden_left / hidden_tombstoned
        // false) before refresh runs (issue #211).
        sqlx_core::query::query(
            "SELECT refresh_room_summary_display(account_id, room_id) \
             FROM room_summaries WHERE account_id = $1",
        )
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(content.rows_affected() + fallback.rows_affected())
    }

    /// Copy current `room_state` display / visibility onto one summary row.
    /// No-op if the room has no summary yet (no events). Used after a watched
    /// state write and after inserting a brand-new summary.
    pub async fn refresh_room_summary_display(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query("SELECT refresh_room_summary_display($1, $2)")
            .bind(account_id)
            .bind(room_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
