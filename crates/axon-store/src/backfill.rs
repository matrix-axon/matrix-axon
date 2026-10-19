//! Store support for the history-backfill engine (M10) and room purge.
//!
//! Two concerns:
//!
//! * The `room_backfill` cursor ([`Store::get_room_backfill`],
//!   [`Store::save_room_backfill`], [`Store::record_room_backfill_failure`],
//!   [`Store::clear_room_backfill_failures`], [`Store::delete_room_backfill`]) —
//!   per-room progress so the backfill engine resumes where it left off across
//!   restarts, plus the failure backoff and parking that keep a failing room from
//!   hammering the homeserver. See ADR 0043 and ADR 0121.
//! * [`Store::purge_room`] — the destructive "forget this room" path used by the
//!   optional purge-on-leave behavior (ADR 0044): delete every stored trace of a
//!   room for one account (events, state, room account-data, backfill cursor,
//!   and the ADR 0095 `room_summaries` row) and enqueue the room's search
//!   documents for removal.

use sqlx_core::from_row::FromRow;
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
    /// Failed pages since the last successful one (ADR 0121).
    pub consecutive_failures: i32,
    /// Whether the room may be paged now: its backoff or park interval, if any,
    /// has passed. Computed against the database clock.
    pub retry_due: bool,
    /// The room has failed too often, or the homeserver refused it, and is only
    /// retried once per park interval (ADR 0121).
    pub parked: bool,
    /// How many times the homeserver has rejected the room's resume token. Unlike
    /// the failure count, a successful page does not reset it (ADR 0121).
    pub token_rejections: i32,
}

/// The columns [`RoomBackfillState`] reads, shared by the single-room and
/// per-account cursor queries so the two cannot drift.
const CURSOR_COLUMNS: &str = "oldest_seen_token, complete, events_backfilled, \
     consecutive_failures, parked, token_rejections, \
     (retry_at IS NULL OR retry_at <= now()) AS retry_due";

impl sqlx_core::from_row::FromRow<'_, PgRow> for RoomBackfillState {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(RoomBackfillState {
            oldest_seen_token: row.try_get("oldest_seen_token")?,
            complete: row.try_get("complete")?,
            events_backfilled: row.try_get("events_backfilled")?,
            consecutive_failures: row.try_get("consecutive_failures")?,
            retry_due: row.try_get("retry_due")?,
            parked: row.try_get("parked")?,
            token_rejections: row.try_get("token_rejections")?,
        })
    }
}

/// One failed backfill page, as [`Store::record_room_backfill_failure`] stores
/// it. The engine decides the class, delay and parking (ADR 0121); the store
/// only records them.
#[derive(Debug, Clone, Copy)]
pub struct RoomBackfillFailure<'a> {
    /// The failure class, kept in `last_failure` for diagnosis.
    pub kind: &'a str,
    /// How long until the room may be paged again. Zero means the next sweep.
    pub retry_after: std::time::Duration,
    /// Whether the room is now parked.
    pub parked: bool,
    /// Drop the saved resume token, so the next page starts from the room's live
    /// timeline end, and count a token rejection. Set only when the homeserver
    /// rejected the token.
    pub clear_token: bool,
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
    /// Of those, how many are parked after repeated failures (ADR 0121).
    pub rooms_parked: i64,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for AccountBackfillProgress {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(AccountBackfillProgress {
            account_id: row.try_get("account_id")?,
            events_total: row.try_get("events_total")?,
            rooms_total: row.try_get("rooms_total")?,
            rooms_backfilled: row.try_get("rooms_backfilled")?,
            rooms_parked: row.try_get("rooms_parked")?,
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
        let sql = format!(
            "SELECT {CURSOR_COLUMNS} FROM room_backfill WHERE account_id = $1 AND room_id = $2"
        );
        let row = sqlx_core::query_as::query_as::<Postgres, RoomBackfillState>(&sql)
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
        let sql =
            format!("SELECT room_id, {CURSOR_COLUMNS} FROM room_backfill WHERE account_id = $1");
        let rows = sqlx_core::query::query(&sql)
            .bind(account_id)
            .fetch_all(&self.pool)
            .await?;
        let mut map = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            let room_id: String = row.try_get("room_id")?;
            map.insert(room_id, RoomBackfillState::from_row(&row)?);
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
                    count(*) FILTER (WHERE bf.parked AND NOT bf.complete) AS rooms_parked, \
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
    /// the first page for a room inserts the row and later pages update it. A
    /// successful page also clears any failure backoff or parking, but not the
    /// token rejection count (ADR 0121).
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
                 events_backfilled = room_backfill.events_backfilled + EXCLUDED.events_backfilled, \
                 consecutive_failures = 0, \
                 last_failure = NULL, \
                 retry_at = NULL, \
                 parked = false",
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

    /// Record a failed backfill page (ADR 0121): count it, note its class, and set
    /// when the room may be paged again. Upsert, so a room whose very first page
    /// fails still gets a row to carry its backoff. The saved token is kept unless
    /// [`clear_token`](RoomBackfillFailure::clear_token) is set, so a retry resumes
    /// from the same position; a cleared token also counts a rejection. A later
    /// successful page ([`save_room_backfill`](Self::save_room_backfill)) clears
    /// all of it except that count.
    pub async fn record_room_backfill_failure(
        &self,
        account_id: Uuid,
        room_id: &str,
        failure: RoomBackfillFailure<'_>,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query(
            "INSERT INTO room_backfill \
                 (account_id, room_id, consecutive_failures, last_failure, retry_at, parked, \
                  token_rejections) \
             VALUES ($1, $2, 1, $3, now() + make_interval(secs => $4), $5, \
                     CASE WHEN $6 THEN 1 ELSE 0 END) \
             ON CONFLICT (account_id, room_id) DO UPDATE SET \
                 consecutive_failures = room_backfill.consecutive_failures + 1, \
                 last_failure = EXCLUDED.last_failure, \
                 retry_at = EXCLUDED.retry_at, \
                 parked = EXCLUDED.parked, \
                 token_rejections = room_backfill.token_rejections + EXCLUDED.token_rejections, \
                 oldest_seen_token = CASE WHEN $6 THEN NULL \
                                          ELSE room_backfill.oldest_seen_token END",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(failure.kind)
        .bind(failure.retry_after.as_secs_f64())
        .bind(failure.parked)
        .bind(failure.clear_token)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Clear a room's failure count, class, backoff and parking without recording
    /// a page, for a room backfill has stopped paging (ADR 0121). The token
    /// rejection count is kept, as on a successful page.
    pub async fn clear_room_backfill_failures(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query(
            "UPDATE room_backfill SET \
                 consecutive_failures = 0, \
                 last_failure = NULL, \
                 retry_at = NULL, \
                 parked = false \
             WHERE account_id = $1 AND room_id = $2",
        )
        .bind(account_id)
        .bind(room_id)
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
        let through_event_id =
            Self::room_purge_watermark(account_id, room_id, false, &self.pool).await?;
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
    /// leaves advance a pending generation; background retries never advance it.
    pub async fn queue_room_purge(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<i64, StoreError> {
        let through = Self::room_purge_watermark(account_id, room_id, true, &self.pool).await?;
        self.purge_wakeup.notify_one();
        Ok(through)
    }

    /// Wake the supervised purge worker as soon as a leave is durably queued.
    pub async fn room_purge_notified(&self) {
        self.purge_wakeup.notified().await;
    }

    pub(crate) async fn room_purge_watermark<'e, E>(
        account_id: Uuid,
        room_id: &str,
        advance: bool,
        executor: E,
    ) -> Result<i64, StoreError>
    where
        E: sqlx_core::executor::Executor<'e, Database = Postgres>,
    {
        let (through_event_id,): (i64,) = sqlx_core::query_as::query_as(
            "INSERT INTO room_purge_intents (account_id, room_id, through_event_id) \
             SELECT $1, $2, CASE WHEN is_called THEN last_value ELSE 0 END FROM events_id_seq \
             ON CONFLICT (account_id, room_id) DO UPDATE SET through_event_id = \
             CASE WHEN $3 THEN GREATEST(room_purge_intents.through_event_id, EXCLUDED.through_event_id) \
                  ELSE room_purge_intents.through_event_id END \
             RETURNING through_event_id",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(advance)
        .fetch_one(executor)
        .await?;
        Ok(through_event_id)
    }

    /// Retry a bounded page; failures remain durable and do not stop other rooms.
    pub async fn retry_room_purges(&self) -> Result<(), StoreError> {
        // Rotate across failed pages instead of letting the first hundred failures
        // hide every later room. The cursor is shared by all clones; durable intents
        // survive restart even though fair traversal starts over after restart.
        let mut cursor = self.purge_retry_cursor.lock().await;
        let pending: Vec<(Uuid, String)> = sqlx_core::query_as::query_as(
            "SELECT account_id, room_id FROM room_purge_intents \
             ORDER BY CASE WHEN $1::uuid IS NULL THEN false \
                           ELSE (account_id, room_id) <= ($1, $2) END, account_id, room_id LIMIT 100")
            .bind(cursor.as_ref().map(|(account, _)| *account))
            .bind(cursor.as_ref().map(|(_, room)| room.as_str()))
            .fetch_all(&self.pool).await?;
        *cursor = pending.last().cloned();
        drop(cursor);
        for (account_id, room_id) in pending {
            if let Err(error) = self.purge_room(account_id, &room_id).await {
                tracing::warn!(%account_id, %room_id, reason = error.diagnostic_reason(), "room purge remains pending");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod purge_watermark_tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn unused_sequence_watermark_is_zero() {
        let store = Store::connect(&std::env::var("DATABASE_URL").unwrap(), 1)
            .await
            .unwrap();
        let account = store
            .upsert_account(
                &format!("@sequence-{}:localhost", Uuid::new_v4()),
                "https://hs.example.org",
            )
            .await
            .unwrap();
        // Check the real column's dependency before the temporary name shadows
        // it: the watermark must track the sequence allocating production IDs.
        let wired: bool = sqlx_core::query_scalar::query_scalar(
            "SELECT pg_get_serial_sequence('events', 'id')::regclass = 'events_id_seq'::regclass",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert!(
            wired,
            "events.id must draw from the purge watermark sequence"
        );
        let mut tx = store.pool().begin().await.unwrap();
        // A temporary relation shadows the production sequence only on this
        // transaction's connection; other tests can keep allocating event IDs.
        sqlx_core::query::query("CREATE TEMP SEQUENCE events_id_seq START WITH 1")
            .execute(&mut *tx)
            .await
            .unwrap();
        let through =
            Store::room_purge_watermark(account.account_id, "!empty:localhost", true, &mut *tx)
                .await
                .unwrap();
        assert_eq!(through, 0);
        let first: i64 = sqlx_core::query_scalar::query_scalar("SELECT nextval('events_id_seq')")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert!(
            first > through,
            "the first post-leave event is outside the purge generation"
        );
        tx.rollback().await.unwrap();
        store.delete_account_row(account.account_id).await.unwrap();
    }
}
