//! Store support for the history-backfill engine (M10) and room purge.
//!
//! Two concerns:
//!
//! * The `room_backfill` cursor ([`Store::get_room_backfill`],
//!   [`Store::save_room_backfill`], [`Store::delete_room_backfill`]) — per-room
//!   progress so the backfill engine resumes where it left off across restarts.
//!   See ADR 0043.
//! * [`Store::purge_room`] — the destructive "forget this room" path used by the
//!   optional purge-on-leave behavior (ADR 0044): delete every stored trace of a
//!   room for one account (events, state, room account-data, backfill cursor,
//!   and the ADR 0095 `room_summaries` row) and enqueue the room's search
//!   documents for removal.

use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::search::room_purge_sentinel;
use crate::{Store, StoreError};

/// A room's backfill progress: where backward paging has reached and whether the
/// room's upstream history is exhausted.
#[derive(Debug, Clone)]
pub struct RoomBackfillState {
    /// The SDK `Messages.end` token from the last page — where to resume backward
    /// paging. `None` before the first page (start from the room's timeline end).
    pub oldest_seen_token: Option<String>,
    /// Upstream history is exhausted (nothing older to fetch); the room is skipped.
    pub complete: bool,
    /// Running count of events backfilled, for the optional bounded target depth.
    pub events_backfilled: i64,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for RoomBackfillState {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(RoomBackfillState {
            oldest_seen_token: row.try_get("oldest_seen_token")?,
            complete: row.try_get("complete")?,
            events_backfilled: row.try_get("events_backfilled")?,
        })
    }
}

/// Account-level backfill progress for `GET /v1/status` — how far the deep-history
/// backfill has gotten for one account, so a client can show whether it is still
/// running or done.
#[derive(Debug, Clone)]
pub struct AccountBackfillProgress {
    /// The account.
    pub account_id: Uuid,
    /// Total events stored for the account.
    pub events_total: i64,
    /// Currently-joined rooms that have any stored events.
    pub rooms_total: i64,
    /// Of those, how many have been backfilled to the room's start (`complete`).
    pub rooms_backfilled: i64,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for AccountBackfillProgress {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(AccountBackfillProgress {
            account_id: row.try_get("account_id")?,
            events_total: row.try_get("events_total")?,
            rooms_total: row.try_get("rooms_total")?,
            rooms_backfilled: row.try_get("rooms_backfilled")?,
        })
    }
}

impl Store {
    /// The backfill cursor for `(account_id, room_id)`, or `None` if the room has
    /// never been backfilled (the engine then starts from the room's timeline end).
    pub async fn get_room_backfill(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<Option<RoomBackfillState>, StoreError> {
        let row = sqlx_core::query_as::query_as::<Postgres, RoomBackfillState>(
            "SELECT oldest_seen_token, complete, events_backfilled \
             FROM room_backfill WHERE account_id = $1 AND room_id = $2",
        )
        .bind(account_id)
        .bind(room_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Every backfill cursor for `account_id`, keyed by room id. The backfill task
    /// loads this once per sweep (one query) instead of a per-room lookup, so a
    /// large account doesn't issue a query per room on every pass.
    pub async fn list_room_backfill(
        &self,
        account_id: Uuid,
    ) -> Result<std::collections::HashMap<String, RoomBackfillState>, StoreError> {
        let rows = sqlx_core::query::query(
            "SELECT room_id, oldest_seen_token, complete, events_backfilled \
             FROM room_backfill WHERE account_id = $1",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        let mut map = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            let room_id: String = row.try_get("room_id")?;
            map.insert(
                room_id,
                RoomBackfillState {
                    oldest_seen_token: row.try_get("oldest_seen_token")?,
                    complete: row.try_get("complete")?,
                    events_backfilled: row.try_get("events_backfilled")?,
                },
            );
        }
        Ok(map)
    }

    /// Per-account backfill progress across the account's currently-joined rooms
    /// (the ADR-0037 leave/ban predicate, same as `list_rooms`). One row per
    /// account that has any stored events. Used by `GET /v1/status` (M10).
    pub async fn backfill_progress(&self) -> Result<Vec<AccountBackfillProgress>, StoreError> {
        let rows = sqlx_core::query_as::query_as::<Postgres, AccountBackfillProgress>(
            "WITH joined AS ( \
                 SELECT DISTINCT e.account_id, e.room_id \
                 FROM events e \
                 JOIN accounts ac ON ac.account_id = e.account_id \
                 WHERE NOT EXISTS ( \
                     SELECT 1 FROM room_state rs \
                       WHERE rs.account_id = e.account_id AND rs.room_id = e.room_id \
                         AND rs.event_type = 'm.room.member' AND rs.state_key = ac.user_id \
                         AND rs.content->>'membership' IN ('leave', 'ban') \
                 ) \
             ) \
             SELECT j.account_id, \
                    count(*) AS rooms_total, \
                    count(*) FILTER (WHERE bf.complete) AS rooms_backfilled, \
                    (SELECT count(*) FROM events e2 WHERE e2.account_id = j.account_id) \
                        AS events_total \
             FROM joined j \
             LEFT JOIN room_backfill bf \
                 ON bf.account_id = j.account_id AND bf.room_id = j.room_id \
             GROUP BY j.account_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Record progress after a backfill page: set the resume token and `complete`
    /// flag, and add `added` to the running `events_backfilled` count. Upsert, so
    /// the first page for a room inserts the row and later pages update it.
    ///
    /// Progress is saved only *after* a page's events are fully persisted, so a
    /// crash mid-page simply re-pages from the previously saved token — idempotent,
    /// because event upserts are `ON CONFLICT DO NOTHING`.
    pub async fn save_room_backfill(
        &self,
        account_id: Uuid,
        room_id: &str,
        oldest_seen_token: Option<&str>,
        complete: bool,
        added: i64,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query(
            "INSERT INTO room_backfill \
                 (account_id, room_id, oldest_seen_token, complete, events_backfilled) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (account_id, room_id) DO UPDATE SET \
                 oldest_seen_token = EXCLUDED.oldest_seen_token, \
                 complete = EXCLUDED.complete, \
                 events_backfilled = room_backfill.events_backfilled + EXCLUDED.events_backfilled",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(oldest_seen_token)
        .bind(complete)
        .bind(added)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete a room's backfill cursor. Used when a room is purged; a later re-join
    /// then backfills it afresh from the timeline end.
    pub async fn delete_room_backfill(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query("DELETE FROM room_backfill WHERE account_id = $1 AND room_id = $2")
            .bind(account_id)
            .bind(room_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Destructively remove every stored trace of `room_id` for `account_id` and
    /// enqueue removal of the room's search documents. One atomic statement: the
    /// event deletes cascade to their crypto siblings (FK `ON DELETE CASCADE`), and
    /// the room-purge obligation is appended to `search_outbox` in the same
    /// transaction — so the store deletion and the index obligation commit together
    /// (the outbox has no FK, so the breadcrumb survives to be applied even if the
    /// indexer is offline). Used by purge-on-leave (ADR 0044).
    ///
    /// `account_data` for the room is deleted by matching `room_id` exactly, which
    /// never touches the global-scope rows (`room_id = ''`).
    ///
    /// Idempotent and cheap to re-run: the search-purge obligation is enqueued
    /// **only when an event was actually deleted**, so re-triggering a purge on an
    /// already-empty room (e.g. the leave state re-observed on a later sync) adds
    /// no `search_outbox` rows and does no indexing work. The matching
    /// `room_summaries` row (ADR 0095) is deleted in the same statement so the
    /// room disappears from `list_rooms` the way deleting its events used to.
    pub async fn purge_room(&self, account_id: Uuid, room_id: &str) -> Result<(), StoreError> {
        sqlx_core::query::query(
            "WITH \
                 del_events AS ( \
                     DELETE FROM events WHERE account_id = $1 AND room_id = $2 RETURNING 1 \
                 ), \
                 del_state AS ( \
                     DELETE FROM room_state WHERE account_id = $1 AND room_id = $2 \
                 ), \
                 del_data AS ( \
                     DELETE FROM account_data WHERE account_id = $1 AND room_id = $2 \
                 ), \
                 del_backfill AS ( \
                     DELETE FROM room_backfill WHERE account_id = $1 AND room_id = $2 \
                 ), \
                 del_summaries AS ( \
                     DELETE FROM room_summaries \
                     WHERE account_id = $1 AND room_id = $2 \
                 ) \
             INSERT INTO search_outbox (account_id, event_id) \
             SELECT $1, $3 WHERE EXISTS (SELECT 1 FROM del_events)",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(room_purge_sentinel(room_id))
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
