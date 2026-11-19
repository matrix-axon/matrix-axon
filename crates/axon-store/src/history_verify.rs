//! Store support for the one-time pass that re-reads each room's stored
//! history against the homeserver (ADR 0140 decision 4, as amended
//! 2026-11-18, #1125).
//!
//! Holes that limited syncs left before ADR 0140 recorded them have no
//! `room_timeline_gaps` row, so the backfill task walks each joined room from
//! its live end and stores every page. `room_history_verify` holds one row per
//! `(account_id, room_id)`, seeded by its migration for every room with stored
//! events. A walk closes at the room's lowest-`id` event, at the floor below
//! stored history, or at the room's start.

use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::backfill::RoomBackfillFailure;
use crate::{Store, StoreError};

/// Why a walk closed. Stored in `room_history_verify.stop_reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyStop {
    /// A page held the room's lowest-`id` event, the first one Axon stored.
    FirstStoredEvent,
    /// The walk read [`FLOOR_RUN`] events in a row that were not stored and
    /// are older than the room's floor: it has passed everything Axon stored,
    /// and the homeserver no longer serves the first stored event.
    Floor,
    /// The homeserver returned no further token.
    RoomStart,
    /// The room had no stored event left when its walk started.
    NoHistory,
    /// [`WALK_TIMEOUTS`] pages timed out over the walk. The homeserver is most
    /// likely fetching history it no longer holds over federation for every
    /// page, which neither the floor nor the backoff bounds.
    TimedOut,
}

impl VerifyStop {
    /// The `stop_reason` column value.
    pub fn as_str(self) -> &'static str {
        match self {
            VerifyStop::FirstStoredEvent => "first_stored_event",
            VerifyStop::Floor => "floor",
            VerifyStop::RoomStart => "room_start",
            VerifyStop::NoHistory => "no_history",
            VerifyStop::TimedOut => "timed_out",
        }
    }
}

/// How many events in a row that were neither already stored nor newer than
/// the room's floor close a walk (ADR 0140 decision 4). Longer than the 103
/// missing in a row that the 2026-10-18 import left, the longest seen.
pub const FLOOR_RUN: i32 = 500;

/// How many empty pages in a row back a walk off (ADR 0121) instead of
/// counting as progress, so a homeserver that keeps answering with empty pages
/// cannot hold the walk open, and parks it in the end.
pub const EMPTY_RUN: i32 = 10;

/// How many timed-out pages over a whole walk close it as
/// [`VerifyStop::TimedOut`]. Counted across successful pages: a homeserver
/// backfilling over federation answers each retry with the page it finished
/// meanwhile, then times out on the next. On Adam's production account one
/// room in 5,816 timed out at all.
pub const WALK_TIMEOUTS: i32 = 3;

/// One incomplete verification row, as the backfill task reads it.
#[derive(Debug, Clone)]
pub struct HistoryVerifyRow {
    pub room_id: String,
    /// The next page's `from`; `None` starts at the room's live end.
    pub from_token: Option<String>,
    /// The room's lowest-`id` event, read when the walk starts; `None` before.
    pub stop_event_ids: Option<Vec<String>>,
    /// The oldest `origin_ts` among the room's `timeline_limit` lowest-`id`
    /// events; `None` before the walk starts, or if the room had none.
    pub floor_ts: Option<i64>,
    /// Events in a row that were neither stored nor newer than the floor.
    pub floor_run: i32,
    /// Empty pages in a row.
    pub empty_run: i32,
    /// Timed-out pages over the whole walk.
    pub walk_timeouts: i32,
    /// Bumped by every restart. Every write the walk makes names it, so a
    /// walk read before a restart cannot overwrite the restarted one.
    pub walk_generation: i64,
    pub pages_fetched: i32,
    pub events_recovered: i64,
    pub consecutive_failures: i32,
    pub parked: bool,
    pub token_rejections: i32,
    /// The row was last seen in a room the account had not joined.
    pub not_joined: bool,
    /// Its backoff or park interval, if any, has passed (database clock).
    pub retry_due: bool,
    /// The room has an open gap row, which is filling the same newest history:
    /// the walk waits for it (ADR 0140 decision 4).
    pub held: bool,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for HistoryVerifyRow {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(HistoryVerifyRow {
            room_id: row.try_get("room_id")?,
            from_token: row.try_get("from_token")?,
            stop_event_ids: row.try_get("stop_event_ids")?,
            floor_ts: row.try_get("floor_ts")?,
            floor_run: row.try_get("floor_run")?,
            empty_run: row.try_get("empty_run")?,
            walk_timeouts: row.try_get("walk_timeouts")?,
            walk_generation: row.try_get("walk_generation")?,
            pages_fetched: row.try_get("pages_fetched")?,
            events_recovered: row.try_get("events_recovered")?,
            consecutive_failures: row.try_get("consecutive_failures")?,
            parked: row.try_get("parked")?,
            token_rejections: row.try_get("token_rejections")?,
            not_joined: row.try_get("not_joined")?,
            retry_due: row.try_get("retry_due")?,
            held: row.try_get("held")?,
        })
    }
}

/// A walk's stop points, read from the room's rows when it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyStops {
    /// The room's lowest-`id` event.
    pub stop_event_ids: Vec<String>,
    pub floor_ts: Option<i64>,
}

/// What [`Store::start_history_verify`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkStart {
    /// The stop points were read and saved.
    Started(VerifyStops),
    /// The room had no stored event left, so the walk was closed as
    /// [`VerifyStop::NoHistory`] in the same statement.
    NoHistory,
    /// The row is gone (the room was purged) or was restarted since it was
    /// read: the caller drops its copy and reads the row again.
    Stale,
}

/// A walk's row as it stands now, read before each page
/// ([`Store::history_verify_guard`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalkGuard {
    pub walk_generation: i64,
    pub complete: bool,
    /// The room has an open gap row.
    pub held: bool,
}

/// A closed walk's totals, for its closing log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyTotals {
    pub pages_fetched: i32,
    pub events_recovered: i64,
}

/// Verification rows across every account, for `GET /v1/status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryVerifyCounts {
    /// Incomplete rows of joined rooms.
    pub rooms_pending: i64,
    /// Of those, rooms waiting on an open gap row.
    pub rooms_held: i64,
    /// Of those, rooms parked after repeated failures (ADR 0121), retried
    /// about once a day.
    pub rooms_parked: i64,
    /// Complete rooms whose walk was closed after [`WALK_TIMEOUTS`] timeouts,
    /// so their history was not fully verified.
    pub rooms_timed_out: i64,
    /// Incomplete rows of rooms the account was not in when last looked at.
    pub rooms_not_joined: i64,
    pub rooms_complete: i64,
    /// Events the walks stored that were missing, summed over every row.
    pub events_recovered: i64,
}

/// The columns [`HistoryVerifyRow`] reads, from `room_history_verify v`.
const ROW_COLUMNS: &str = "v.room_id, v.from_token, v.stop_event_ids, v.floor_ts, v.floor_run, \
     v.empty_run, v.walk_timeouts, v.walk_generation, v.pages_fetched, v.events_recovered, v.consecutive_failures, v.parked, \
     v.token_rejections, v.not_joined, \
     (v.retry_at IS NULL OR v.retry_at <= now()) AS retry_due, \
     EXISTS (SELECT 1 FROM room_timeline_gaps g \
             WHERE g.account_id = v.account_id AND g.room_id = v.room_id) AS held";

/// What restarting a walk clears: its position and stop points, and its
/// failure state. It bumps the generation, so a walk already in flight finds
/// its writes refused and reads the row again. `queued_at` records when.
const RESTART_WALK: &str = "from_token = NULL, stop_event_ids = NULL, floor_ts = NULL, \
     floor_run = 0, empty_run = 0, walk_timeouts = 0, \
     walk_generation = room_history_verify.walk_generation + 1, \
     complete = false, stop_reason = NULL, completed_at = NULL, \
     consecutive_failures = 0, last_failure = NULL, retry_at = NULL, parked = false, \
     token_rejections = 0, queued_at = now()";

impl Store {
    /// The account's incomplete verification rows, most recently active room
    /// first: holes are likeliest in recent history.
    pub async fn list_history_verify(
        &self,
        account_id: Uuid,
    ) -> Result<Vec<HistoryVerifyRow>, StoreError> {
        let sql = format!(
            "SELECT {ROW_COLUMNS} FROM room_history_verify v \
             LEFT JOIN room_summaries s \
               ON s.account_id = v.account_id AND s.room_id = v.room_id \
             WHERE v.account_id = $1 AND NOT v.complete \
             ORDER BY s.last_activity_ts DESC NULLS LAST, v.room_id"
        );
        let rows = sqlx_core::query_as::query_as::<Postgres, HistoryVerifyRow>(&sql)
            .bind(account_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// Record whether the account is in these rooms, for status. Called with
    /// only the rows whose flag changed, so a steady account writes nothing.
    pub async fn mark_history_verify_joined(
        &self,
        account_id: Uuid,
        room_ids: &[String],
        joined: bool,
    ) -> Result<(), StoreError> {
        if room_ids.is_empty() {
            return Ok(());
        }
        sqlx_core::query::query(
            "UPDATE room_history_verify SET not_joined = $3 \
             WHERE account_id = $1 AND room_id = ANY($2)",
        )
        .bind(account_id)
        .bind(room_ids)
        .bind(!joined)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The walk's row as it stands now: its generation, whether it is
    /// complete, and whether the room has an open gap row. Read before each
    /// page, so a walk restarted or held since its row was listed is not
    /// paged. `None` if the row is gone.
    pub async fn history_verify_guard(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<Option<WalkGuard>, StoreError> {
        let row: Option<(i64, bool, bool)> = sqlx_core::query_as::query_as(
            "SELECT v.walk_generation, v.complete, \
                    EXISTS (SELECT 1 FROM room_timeline_gaps g \
                            WHERE g.account_id = v.account_id AND g.room_id = v.room_id) \
             FROM room_history_verify v WHERE v.account_id = $1 AND v.room_id = $2",
        )
        .bind(account_id)
        .bind(room_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(walk_generation, complete, held)| WalkGuard {
            walk_generation,
            complete,
            held,
        }))
    }

    /// Read a walk's stop points from the room's rows and save them, from the
    /// live end: the room's lowest-`id` event, and the oldest `origin_ts` among
    /// its `timeline_limit` lowest-`id` events. A room with no stored event
    /// left is closed as [`VerifyStop::NoHistory`] in the same statement, so a
    /// crash cannot leave a started walk with nothing to stop at.
    ///
    /// The floor reads several events rather than the stop event alone, so
    /// one event with a wrong clock cannot raise it.
    pub async fn start_history_verify(
        &self,
        account_id: Uuid,
        room_id: &str,
        walk_generation: i64,
        timeline_limit: u32,
    ) -> Result<WalkStart, StoreError> {
        let row: Option<(Vec<String>, Option<i64>)> = sqlx_core::query_as::query_as(
            "WITH s AS ( \
                 SELECT ARRAY(SELECT e.event_id FROM events e \
                              WHERE e.account_id = $1 AND e.room_id = $2 \
                              ORDER BY e.id LIMIT 1) AS ids, \
                        (SELECT min(f.origin_ts) FROM ( \
                             SELECT e.origin_ts FROM events e \
                             WHERE e.account_id = $1 AND e.room_id = $2 \
                             ORDER BY e.id LIMIT $4) f) AS floor \
             ) \
             UPDATE room_history_verify v SET \
                 stop_event_ids = s.ids, \
                 floor_ts = s.floor, \
                 floor_run = 0, \
                 empty_run = 0, \
                 from_token = NULL, \
                 complete = cardinality(s.ids) = 0, \
                 stop_reason = CASE WHEN cardinality(s.ids) = 0 THEN 'no_history' END, \
                 completed_at = CASE WHEN cardinality(s.ids) = 0 THEN now() END \
             FROM s \
             WHERE v.account_id = $1 AND v.room_id = $2 AND v.walk_generation = $3 \
               AND NOT v.complete \
             RETURNING v.stop_event_ids, v.floor_ts",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(walk_generation)
        .bind(i64::from(timeline_limit.max(1)))
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            None => WalkStart::Stale,
            Some((ids, _)) if ids.is_empty() => WalkStart::NoHistory,
            Some((stop_event_ids, floor_ts)) => WalkStart::Started(VerifyStops {
                stop_event_ids,
                floor_ts,
            }),
        })
    }

    /// Look up a page's event ids before the page is written: which are
    /// already stored, and whether the page holds a stop event.
    pub async fn lookup_verify_page(
        &self,
        account_id: Uuid,
        page_event_ids: &[String],
        stop_event_ids: &[String],
    ) -> Result<VerifyPageLookup, StoreError> {
        let stored: Vec<(String,)> = sqlx_core::query_as::query_as(
            "SELECT event_id FROM events WHERE account_id = $1 AND event_id = ANY($2)",
        )
        .bind(account_id)
        .bind(page_event_ids)
        .fetch_all(&self.pool)
        .await?;
        let stored: std::collections::HashSet<String> =
            stored.into_iter().map(|(id,)| id).collect();
        let reaches_stop = stop_event_ids.iter().any(|id| stored.contains(id));
        Ok(VerifyPageLookup {
            stored,
            reaches_stop,
        })
    }

    /// Save progress after a page that did not close the walk: resume from
    /// `progress.end`, count the page and what it inserted, and keep the floor
    /// and empty runs. A page with events also clears any failure backoff; an
    /// empty one does not, so a homeserver that keeps answering with empty
    /// pages still parks the room. `false` if the walk was restarted or its row
    /// is gone since it was read: nothing was written.
    pub async fn save_history_verify_page(
        &self,
        account_id: Uuid,
        room_id: &str,
        walk_generation: i64,
        progress: WalkProgress<'_>,
    ) -> Result<bool, StoreError> {
        let done = sqlx_core::query::query(
            "UPDATE room_history_verify SET \
                 from_token = $4, \
                 pages_fetched = pages_fetched + 1, \
                 events_recovered = events_recovered + $5, \
                 floor_run = $6, \
                 empty_run = $7, \
                 consecutive_failures = CASE WHEN $8 THEN 0 ELSE consecutive_failures END, \
                 last_failure = CASE WHEN $8 THEN NULL ELSE last_failure END, \
                 retry_at = CASE WHEN $8 THEN NULL ELSE retry_at END, \
                 parked = CASE WHEN $8 THEN false ELSE parked END \
             WHERE account_id = $1 AND room_id = $2 AND walk_generation = $3 \
               AND NOT complete",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(walk_generation)
        .bind(progress.end)
        .bind(progress.inserted)
        .bind(progress.floor_run)
        .bind(progress.empty_run)
        .bind(progress.clears_failures)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Close a walk. `paged` says whether a page was fetched to close it.
    /// Returns the walk's totals, or `None` if the walk was restarted or its
    /// row is gone since it was read: nothing was written.
    pub async fn complete_history_verify(
        &self,
        account_id: Uuid,
        room_id: &str,
        walk_generation: i64,
        stop: VerifyStop,
        paged: bool,
        inserted: i64,
    ) -> Result<Option<VerifyTotals>, StoreError> {
        let row: Option<(i32, i64)> = sqlx_core::query_as::query_as(
            "UPDATE room_history_verify SET \
                 complete = true, \
                 stop_reason = $4, \
                 completed_at = now(), \
                 from_token = NULL, \
                 pages_fetched = pages_fetched + $5, \
                 events_recovered = events_recovered + $6, \
                 consecutive_failures = 0, \
                 last_failure = NULL, \
                 retry_at = NULL, \
                 parked = false \
             WHERE account_id = $1 AND room_id = $2 AND walk_generation = $3 \
               AND NOT complete \
             RETURNING pages_fetched, events_recovered",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(walk_generation)
        .bind(stop.as_str())
        .bind(i32::from(paged))
        .bind(inserted)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(pages_fetched, events_recovered)| VerifyTotals {
            pages_fetched,
            events_recovered,
        }))
    }

    /// Record a failed page against a walk (ADR 0121). With
    /// [`clear_token`](RoomBackfillFailure::clear_token), the homeserver
    /// rejected the saved token: the walk restarts from the live end with the
    /// same stop points, and the rejection is counted so a second one parks
    /// it. The empty run starts over, so each attempt gets its own
    /// [`EMPTY_RUN`]. A timeout adds to the walk's
    /// [`walk_timeouts`](HistoryVerifyRow::walk_timeouts). `false` if the walk was restarted or its row is gone
    /// since it was read: nothing was written.
    pub async fn record_history_verify_failure(
        &self,
        account_id: Uuid,
        room_id: &str,
        walk_generation: i64,
        failure: RoomBackfillFailure<'_>,
    ) -> Result<bool, StoreError> {
        let done = sqlx_core::query::query(
            "UPDATE room_history_verify SET \
                 consecutive_failures = consecutive_failures + 1, \
                 last_failure = $4, \
                 retry_at = now() + make_interval(secs => $5), \
                 parked = $6, \
                 from_token = CASE WHEN $7 THEN NULL ELSE from_token END, \
                 floor_run = CASE WHEN $7 THEN 0 ELSE floor_run END, \
                 empty_run = 0, \
                 walk_timeouts = walk_timeouts + CASE WHEN $8 THEN 1 ELSE 0 END, \
                 token_rejections = token_rejections + CASE WHEN $7 THEN 1 ELSE 0 END \
             WHERE account_id = $1 AND room_id = $2 AND walk_generation = $3 \
               AND NOT complete",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(walk_generation)
        .bind(failure.kind)
        .bind(failure.retry_after.as_secs_f64())
        .bind(failure.parked)
        .bind(failure.clear_token)
        .bind(failure.timed_out)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Queue the verification pass again for one room, or for every room of
    /// the account with stored events (the management action, ADR 0140
    /// decision 7). A queued room's walk restarts from its live end and takes
    /// fresh stop points from its rows when its first page is fetched.
    /// Returns how many rooms were queued; a room with no stored event is
    /// not.
    ///
    /// On the maintenance pool: for a whole account it reads every stored
    /// event's room, several seconds on a large account (7 s for 2.5 M
    /// events), too close to the main pool's statement timeout.
    pub async fn requeue_history_verify(
        &self,
        account_id: Uuid,
        room_id: Option<&str>,
    ) -> Result<u64, StoreError> {
        let sql = format!(
            "INSERT INTO room_history_verify (account_id, room_id) \
             SELECT DISTINCT account_id, room_id FROM events \
             WHERE account_id = $1 AND ($2::text IS NULL OR room_id = $2) \
             ON CONFLICT (account_id, room_id) DO UPDATE SET {RESTART_WALK}, \
                 pages_fetched = 0, events_recovered = 0"
        );
        let done = sqlx_core::query::query(&sql)
            .bind(account_id)
            .bind(room_id)
            .execute(&self.maintenance_pool)
            .await?;
        Ok(done.rows_affected())
    }

    /// Verification-row counts across every account, for `GET /v1/status`.
    /// On the isolated status pool, like
    /// [`history_gap_counts`](Self::history_gap_counts).
    pub async fn history_verify_counts(&self) -> Result<HistoryVerifyCounts, StoreError> {
        type Counts = (i64, i64, i64, i64, i64, i64, i64);
        let (
            rooms_pending,
            rooms_held,
            rooms_parked,
            rooms_timed_out,
            rooms_not_joined,
            rooms_complete,
            events_recovered,
        ): Counts = sqlx_core::query_as::query_as(
            "SELECT count(*) FILTER (WHERE NOT v.complete AND NOT v.not_joined), \
                    count(*) FILTER (WHERE NOT v.complete AND NOT v.not_joined \
                                       AND g.room_id IS NOT NULL), \
                    count(*) FILTER (WHERE NOT v.complete AND NOT v.not_joined AND v.parked), \
                    count(*) FILTER (WHERE v.complete AND v.stop_reason = 'timed_out'), \
                    count(*) FILTER (WHERE NOT v.complete AND v.not_joined), \
                    count(*) FILTER (WHERE v.complete), \
                    coalesce(sum(v.events_recovered), 0)::bigint \
             FROM room_history_verify v \
             LEFT JOIN (SELECT DISTINCT account_id, room_id FROM room_timeline_gaps) g \
               ON g.account_id = v.account_id AND g.room_id = v.room_id",
        )
        .fetch_one(&self.status_pool)
        .await?;
        Ok(HistoryVerifyCounts {
            rooms_pending,
            rooms_held,
            rooms_parked,
            rooms_timed_out,
            rooms_not_joined,
            rooms_complete,
            events_recovered,
        })
    }
}

/// What [`Store::save_history_verify_page`] saves for a page that did not
/// close the walk.
#[derive(Debug, Clone, Copy)]
pub struct WalkProgress<'a> {
    /// The next page's `from`.
    pub end: &'a str,
    /// Events the page stored that were missing.
    pub inserted: i64,
    pub floor_run: i32,
    pub empty_run: i32,
    /// The page had events, so the homeserver is serving the walk: clear any
    /// failure backoff.
    pub clears_failures: bool,
}

/// What a page's events already were in `events`, looked up before the page
/// is written ([`Store::lookup_verify_page`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyPageLookup {
    /// The page's events that were already stored.
    pub stored: std::collections::HashSet<String>,
    /// The page holds the walk's stop event.
    pub reaches_stop: bool,
}

/// The SQL that restarts an incomplete walk in a room a purge kept (ADR 0140
/// decision 6), for `purge_room`'s closing statement.
pub(crate) fn restart_kept_walk_sql() -> String {
    format!(
        "UPDATE room_history_verify SET {RESTART_WALK} \
         WHERE account_id = $1 AND room_id = $2 AND NOT complete \
           AND preserve_room_after_purge($1, $2, $3)"
    )
}

/// The SQL that re-queues a gap row's room for verification as the row is
/// deleted (#1170): one statement, so the hole is never left with neither.
pub(crate) fn requeue_on_gap_delete_sql() -> String {
    format!(
        "WITH d AS (DELETE FROM room_timeline_gaps WHERE id = $1 \
                    RETURNING account_id, room_id) \
         INSERT INTO room_history_verify (account_id, room_id) \
         SELECT account_id, room_id FROM d \
         ON CONFLICT (account_id, room_id) DO UPDATE SET {RESTART_WALK}"
    )
}
