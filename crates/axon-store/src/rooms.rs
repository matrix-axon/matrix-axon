//! Room summaries: the cross-account room list that backs `GET /v1/rooms`.
//!
//! There is no `rooms` table — a "room" is derived. Activity (the sort key) and
//! the latest event id come from aggregating [`events`](crate::events); the
//! display fields (name, topic, avatar, canonical alias) are point lookups into
//! the resolved [`room_state`](crate::state) projection. A room joined by two
//! accounts yields two summaries, one per `account_id` — the natural identity
//! here is `(account_id, room_id)`.

use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::{Store, StoreError};

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
    /// `MAX(origin_ts)` over the room's events, in milliseconds — the sort key.
    pub last_activity_ts: i64,
    /// The `event_id` at `last_activity_ts` (latest by `(origin_ts, id)`).
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
    /// One query: a per-`(account_id, room_id)` aggregate over `events` supplies
    /// the activity timestamp and the latest event id, five correlated
    /// sub-selects pull the display fields from the `room_state` projection by
    /// its primary key (so they are single-row index lookups, not scans), and
    /// two more correlated sub-selects into `room_unread_counts` (issue #313,
    /// ADR 0070) supply the unread counts — likewise a primary-key lookup, not
    /// an aggregate, so this doesn't reintroduce the cost ADR 0055 deferred.
    /// `COALESCE(..., 0)` covers a room with no `room_unread_counts` row yet
    /// (between account creation and the sync engine's first sweep), so the
    /// fields read as `0` rather than `NULL`.
    ///
    /// Rooms the local user has left or been banned from are excluded: a
    /// correlated `NOT EXISTS` drops any room whose current `m.room.member`
    /// membership for `ac.user_id` is `leave` or `ban` (ADR 0037). The leave
    /// event reaches `room_state` through ordinary sync, so no leave-specific
    /// write path is needed. The predicate hides only on a definitive
    /// leave/ban signal — a room with no membership row for the local user
    /// still appears, so missing membership data never hides a joined room.
    ///
    /// Tombstoned rooms (upgraded via `m.room.tombstone`) are also excluded:
    /// the old room is superseded by its replacement and should not appear
    /// alongside the new room in the list.
    pub async fn list_rooms(
        &self,
        account_id: Option<Uuid>,
    ) -> Result<Vec<RoomSummary>, StoreError> {
        let rows = sqlx_core::query_as::query_as::<Postgres, RoomSummary>(
            "SELECT a.account_id, ac.user_id AS account_user_id, a.room_id, \
                    a.last_activity_ts, a.last_event_id, \
                    (SELECT rs.content->>'name'  FROM room_state rs \
                       WHERE rs.account_id = a.account_id AND rs.room_id = a.room_id \
                         AND rs.event_type = 'm.room.name' AND rs.state_key = '') AS name, \
                    (SELECT rs.content->>'topic' FROM room_state rs \
                       WHERE rs.account_id = a.account_id AND rs.room_id = a.room_id \
                         AND rs.event_type = 'm.room.topic' AND rs.state_key = '') AS topic, \
                    (SELECT rs.content->>'url'   FROM room_state rs \
                       WHERE rs.account_id = a.account_id AND rs.room_id = a.room_id \
                         AND rs.event_type = 'm.room.avatar' AND rs.state_key = '') AS avatar_url, \
                    (SELECT rs.content->>'alias' FROM room_state rs \
                       WHERE rs.account_id = a.account_id AND rs.room_id = a.room_id \
                         AND rs.event_type = 'm.room.canonical_alias' AND rs.state_key = '') \
                         AS canonical_alias, \
                    (SELECT rs.content->>'type' FROM room_state rs \
                       WHERE rs.account_id = a.account_id AND rs.room_id = a.room_id \
                         AND rs.event_type = 'm.room.create' AND rs.state_key = '') AS room_type, \
                    COALESCE((SELECT ruc.notification_count FROM room_unread_counts ruc \
                       WHERE ruc.account_id = a.account_id AND ruc.room_id = a.room_id), 0) \
                       AS notification_count, \
                    COALESCE((SELECT ruc.highlight_count FROM room_unread_counts ruc \
                       WHERE ruc.account_id = a.account_id AND ruc.room_id = a.room_id), 0) \
                       AS highlight_count \
             FROM ( \
                 SELECT account_id, room_id, \
                        MAX(origin_ts) AS last_activity_ts, \
                        (array_agg(event_id ORDER BY origin_ts DESC, id DESC))[1] AS last_event_id \
                 FROM events \
                 WHERE ($1::uuid IS NULL OR account_id = $1) \
                 GROUP BY account_id, room_id \
             ) a \
             JOIN accounts ac ON ac.account_id = a.account_id AND ac.state = 'active' \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM room_state rs \
                   WHERE rs.account_id = a.account_id AND rs.room_id = a.room_id \
                     AND rs.event_type = 'm.room.member' AND rs.state_key = ac.user_id \
                     AND rs.content->>'membership' IN ('leave', 'ban') \
             ) \
             AND NOT EXISTS ( \
                 SELECT 1 FROM room_state rs \
                   WHERE rs.account_id = a.account_id AND rs.room_id = a.room_id \
                     AND rs.event_type = 'm.room.tombstone' AND rs.state_key = '' \
             ) \
             ORDER BY a.last_activity_ts DESC, a.room_id",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
