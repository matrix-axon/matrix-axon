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
    /// account with stored events in a joined room. Used by `GET /v1/status` (M10).
    /// Rooms come from the incrementally maintained summaries, so membership is
    /// checked once per room rather than once per event. Exact event counts are
    /// maintained by database triggers; the isolated status deadline bounds queries,
    /// including when a caller disconnects.
    pub async fn backfill_progress(&self) -> Result<Vec<AccountBackfillProgress>, StoreError> {
        let rows = sqlx_core::query_as::query_as::<Postgres, AccountBackfillProgress>(
            "WITH joined AS ( \
                 SELECT s.account_id, s.room_id, ac.events_total \
                 FROM room_summaries s \
                 JOIN accounts ac ON ac.account_id = s.account_id \
                 WHERE EXISTS ( \
                     SELECT 1 FROM events e \
                     WHERE e.account_id = s.account_id AND e.room_id = s.room_id \
                 ) AND NOT EXISTS ( \
                     SELECT 1 FROM room_state rs \
                       WHERE rs.account_id = s.account_id AND rs.room_id = s.room_id \
                         AND rs.event_type = 'm.room.member' AND rs.state_key = ac.user_id \
                         AND rs.content->>'membership' IN ('leave', 'ban') \
                 ) \
             ) \
             SELECT j.account_id, \
                    count(*) AS rooms_total, \
                    count(*) FILTER (WHERE bf.complete) AS rooms_backfilled, \
                    max(j.events_total) AS events_total \
             FROM joined j \
             LEFT JOIN room_backfill bf \
                 ON bf.account_id = j.account_id AND bf.room_id = j.room_id \
             GROUP BY j.account_id",
        )
        .fetch_all(&self.status_pool)
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

    /// Purge a room in committed event batches, with an atomic search obligation
    /// per batch. A durable intent survives interruption and is retried by the sync
    /// engine. Room metadata and the intent are removed together after the events.
    /// The durable event watermark and per-event index obligations preserve data
    /// received after the leave, including a later rejoin.
    /// Global account data (`room_id = ''`) is preserved. Idempotent.
    pub async fn purge_room(&self, account_id: Uuid, room_id: &str) -> Result<(), StoreError> {
        let through_event_id = self.queue_room_purge(account_id, room_id).await?;
        self.delete_event_batches(account_id, Some(room_id), Some(through_event_id))
            .await?;
        self.delete_state_batches(account_id, Some(room_id), Some(through_event_id))
            .await?;
        // Re-derive activity from remaining events, including a later rejoin.
        self.rebuild_room_summaries_for(account_id, Some(room_id))
            .await?;
        sqlx_core::query::query(
            "WITH del_backfill AS ( \
                DELETE FROM room_backfill WHERE account_id = $1 AND room_id = $2 \
                AND NOT preserve_room_after_purge($1, $2, $3) \
             ) DELETE FROM room_purge_intents WHERE account_id = $1 AND room_id = $2 \
             AND through_event_id = $3",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(through_event_id)
        .execute(&self.maintenance_pool)
        .await?;
        Ok(())
    }
    /// Persist a leave obligation without waiting for bulk cleanup. Repeated
    /// requests preserve the original generation until it finishes.
    pub async fn queue_room_purge(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<i64, StoreError> {
        let (through_event_id,): (i64,) = sqlx_core::query_as::query_as(
            "INSERT INTO room_purge_intents (account_id, room_id, through_event_id) \
             SELECT $1, $2, last_value FROM events_id_seq \
             ON CONFLICT (account_id, room_id) DO UPDATE SET room_id = EXCLUDED.room_id \
             RETURNING through_event_id",
        )
        .bind(account_id)
        .bind(room_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(through_event_id)
    }

    /// Retry a bounded page; failures remain durable and do not stop other rooms.
    pub async fn retry_room_purges(&self) -> Result<(), StoreError> {
        let pending: Vec<(Uuid, String)> = sqlx_core::query_as::query_as(
            "SELECT account_id, room_id FROM room_purge_intents ORDER BY account_id, room_id LIMIT 100")
            .fetch_all(&self.pool).await?;
        for (account_id, room_id) in pending {
            if let Err(error) = self.purge_room(account_id, &room_id).await {
                tracing::warn!(%account_id, %room_id, reason = error.diagnostic_reason(), "room purge remains pending");
            }
        }
        Ok(())
    }
}
