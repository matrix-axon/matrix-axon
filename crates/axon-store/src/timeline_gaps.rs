//! Store support for filling the holes that limited syncs leave in a room's
//! history (ADR 0140, #1125).
//!
//! The room-update watcher records a row in `room_timeline_gaps` for every
//! limited sliding-sync response ([`Store::record_limited_gap`]) and, when its
//! broadcast lags, a probe for every room that stored events during the lag
//! ([`Store::record_lag_probes`]). The backfill task pages each row backward
//! until it meets history stored before the hole.
//!
//! Every stop decision compares `events.received_at`, the time a row was first
//! stored, which `ON CONFLICT DO NOTHING` never changes. Nothing here orders by
//! `origin_ts`: imported events carry timestamps years older than the events
//! stored around them.

use chrono::{DateTime, Utc};
use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::backfill::RoomBackfillFailure;
use crate::{Store, StoreError};

/// Why a gap row exists. Stored in `room_timeline_gaps.origin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapOrigin {
    /// A limited sliding-sync response.
    Limited,
    /// The watcher's room-update broadcast lagged, so a room that stored events
    /// during the lag is walked back to history stored before it.
    LagProbe,
    /// The homeserver rejected a saved token, so the row restarted from the
    /// live end.
    TokenRejected,
}

impl GapOrigin {
    /// The `origin` column value.
    pub fn as_str(self) -> &'static str {
        match self {
            GapOrigin::Limited => "limited",
            GapOrigin::LagProbe => "lag_probe",
            GapOrigin::TokenRejected => "token_rejected",
        }
    }

    fn parse(value: &str) -> Result<Self, sqlx_core::Error> {
        match value {
            "limited" => Ok(GapOrigin::Limited),
            "lag_probe" => Ok(GapOrigin::LagProbe),
            "token_rejected" => Ok(GapOrigin::TokenRejected),
            other => Err(sqlx_core::Error::Decode(
                format!("unknown room_timeline_gaps.origin {other:?}").into(),
            )),
        }
    }
}

/// One open gap row, as the filler reads it.
#[derive(Debug, Clone)]
pub struct TimelineGap {
    pub id: i64,
    pub room_id: String,
    pub origin: GapOrigin,
    /// The next page's `from`; `None` starts at the room's live end.
    pub from_token: Option<String>,
    /// The stop boundary. `None` only before the first-window check has
    /// resolved a limited row's.
    pub stored_before: Option<DateTime<Utc>>,
    /// The first-window check has run and found a hole.
    pub batch_checked: bool,
    /// A lag probe's extra stop: the room's lowest-id event when it was
    /// recorded.
    pub stop_event_ids: Vec<String>,
    pub pages_fetched: i32,
    pub events_recovered: i64,
    pub consecutive_failures: i32,
    pub parked: bool,
    /// Whether the row may be paged now: its backoff or park interval, if any,
    /// has passed. Computed against the database clock.
    pub retry_due: bool,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for TimelineGap {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        let origin: String = row.try_get("origin")?;
        Ok(TimelineGap {
            id: row.try_get("id")?,
            room_id: row.try_get("room_id")?,
            origin: GapOrigin::parse(&origin)?,
            from_token: row.try_get("from_token")?,
            stored_before: row.try_get("stored_before")?,
            batch_checked: row.try_get("batch_checked")?,
            stop_event_ids: row.try_get("stop_event_ids")?,
            pages_fetched: row.try_get("pages_fetched")?,
            events_recovered: row.try_get("events_recovered")?,
            consecutive_failures: row.try_get("consecutive_failures")?,
            parked: row.try_get("parked")?,
            retry_due: row.try_get("retry_due")?,
        })
    }
}

/// The first-window check's verdict on a row that had not been checked
/// ([`Store::check_timeline_gap`], ADR 0140 decision 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GapCheck {
    /// None of the batch's events is stored yet. The row waits and is checked
    /// again on the next sweep.
    AwaitingBatch,
    /// None of the batch's events was stored within the wait limit, so the
    /// batch will never be stored. The row was deleted unpaged, and the room's
    /// verification walk queued again to find the hole (#1170).
    Expired { from_token: Option<String> },
    /// Nothing was stored in the room before the batch: the room's first window,
    /// which is backfill's job, not a hole. The row was deleted unpaged.
    FirstWindow,
    /// A hole. The row is marked checked, with its boundary resolved.
    Hole,
    /// The row no longer exists (the room was purged meanwhile).
    Gone,
}

/// What a page's events already were in `events`, looked up before the page
/// is written ([`Store::lookup_gap_page`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GapPageLookup {
    /// How many of the page's events were already stored.
    pub already_stored: i64,
    /// Whether the page reaches history stored before the hole: an event stored
    /// before the row's boundary, or one of a lag probe's stop events. The row
    /// closes after this page.
    pub reaches_stored_history: bool,
}

/// A closed row's totals, for its closing log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GapTotals {
    pub pages_fetched: i32,
    pub events_recovered: i64,
}

/// Gap rows across every account, for `GET /v1/status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryGapCounts {
    /// Rows still recorded: holes not yet closed, including parked ones and
    /// ones awaiting their batch.
    pub open: i64,
    /// Of those, rows parked after repeated failures (ADR 0121).
    pub parked: i64,
    /// Of those, unchecked rows none of whose batch is stored yet.
    pub awaiting_batch: i64,
}

/// Where a lag starts (ADR 0140 decision 3): every event a lost sync response
/// stored has a `received_at` at or after `received_at`, and an `id` above
/// `max_event_id`. The id bound lets the probe query range-scan the primary
/// key instead of reading every row's `received_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LagMark {
    pub received_at: DateTime<Utc>,
    pub max_event_id: i64,
}

impl LagMark {
    /// The earlier of two marks in both fields, which covers both lags.
    pub fn earliest(self, other: LagMark) -> LagMark {
        LagMark {
            received_at: self.received_at.min(other.received_at),
            max_event_id: self.max_event_id.min(other.max_event_id),
        }
    }
}

/// What [`Store::check_timeline_gap`] reads for one row: the batch's earliest
/// storage time, whether the wait limit has passed, the first-window verdict,
/// and the row's `from_token`.
type CheckRow = (Option<DateTime<Utc>>, bool, bool, Option<String>);

/// The columns [`TimelineGap`] reads.
const GAP_COLUMNS: &str = "id, room_id, origin, from_token, stored_before, batch_checked, \
     stop_event_ids, pages_fetched, events_recovered, consecutive_failures, parked, \
     (retry_at IS NULL OR retry_at <= now()) AS retry_due";

impl Store {
    /// The database clock and the highest event id allocated so far. The gap
    /// watcher reads it once when it subscribes, before the sync service
    /// starts, as the lag boundary of last resort (ADR 0140 decision 3): every
    /// event a response stores later has a later `received_at` and a higher id.
    pub async fn lag_mark_now(&self) -> Result<LagMark, StoreError> {
        let (received_at, max_event_id): (DateTime<Utc>, i64) = sqlx_core::query_as::query_as(
            "SELECT now(), CASE WHEN is_called THEN last_value ELSE 0 END FROM events_id_seq",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(LagMark {
            received_at,
            max_event_id,
        })
    }

    /// Record the hole a limited response left in `room_id`. With a
    /// `prev_batch`, paging starts there; without one, at the live end. Either
    /// way the boundary is resolved when the row is checked
    /// ([`check_timeline_gap`](Self::check_timeline_gap)), to the earliest
    /// `received_at` among the batch's stored events: the events before the
    /// hole were stored by an earlier response, so they are earlier still, and
    /// the time does not depend on when this insert ran.
    pub async fn record_limited_gap(
        &self,
        account_id: Uuid,
        room_id: &str,
        prev_batch: Option<&str>,
        batch_event_ids: &[String],
    ) -> Result<i64, StoreError> {
        let (id,): (i64,) = sqlx_core::query_as::query_as(
            "INSERT INTO room_timeline_gaps \
                 (account_id, room_id, origin, from_token, batch_event_ids) \
             VALUES ($1, $2, 'limited', $3, $4) \
             RETURNING id",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(prev_batch)
        .bind(batch_event_ids)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// The latest `received_at` and the highest `id` among the stored events
    /// with these ids, or `None` when none of them is stored.
    pub async fn lag_mark(
        &self,
        account_id: Uuid,
        event_ids: &[String],
    ) -> Result<Option<LagMark>, StoreError> {
        let (received_at, max_event_id): (Option<DateTime<Utc>>, Option<i64>) =
            sqlx_core::query_as::query_as(
                "SELECT max(received_at), max(id) FROM events \
                 WHERE account_id = $1 AND event_id = ANY($2)",
            )
            .bind(account_id)
            .bind(event_ids)
            .fetch_one(&self.pool)
            .await?;
        Ok(received_at
            .zip(max_event_id)
            .map(|(received_at, max_event_id)| LagMark {
                received_at,
                max_event_id,
            }))
    }

    /// Record a lag probe for every room in `joined_room_ids` that stored an
    /// event after `mark`, returning the rooms probed. Each probe walks from
    /// the live end back to an event stored before `mark.received_at`, or to
    /// the room's lowest-id event, whichever comes first (ADR 0140 decision 3).
    ///
    /// The lowest-id event is the first event Axon stored in the room, so it
    /// precedes every hole that a later response could leave, however short
    /// the room's first window was. A room first seen during the lag stops
    /// there instead of walking to its start. (Several lowest-id events would
    /// not do: when the first window is shorter than that, they include events
    /// of the later responses, above the holes.)
    ///
    /// Rooms are found by events with an `id` above `mark.max_event_id`, a
    /// primary-key range, and a `received_at` at or after the boundary. Every
    /// event a lost response stored satisfies both, since it was inserted after
    /// the marked events were.
    pub async fn record_lag_probes(
        &self,
        account_id: Uuid,
        mark: LagMark,
        joined_room_ids: &[String],
    ) -> Result<Vec<String>, StoreError> {
        let rooms: Vec<(String,)> = sqlx_core::query_as::query_as(
            "INSERT INTO room_timeline_gaps \
                 (account_id, room_id, origin, stored_before, batch_checked, stop_event_ids) \
             SELECT $1, r.room_id, 'lag_probe', $2, true, \
                    ARRAY(SELECT f.event_id FROM events f \
                          WHERE f.account_id = $1 AND f.room_id = r.room_id \
                          ORDER BY f.id LIMIT 1) \
             FROM (SELECT DISTINCT e.room_id FROM events e \
                   WHERE e.id > $4 AND e.account_id = $1 AND e.received_at >= $2 \
                     AND e.room_id = ANY($3)) r \
             RETURNING room_id",
        )
        .bind(account_id)
        .bind(mark.received_at)
        .bind(joined_room_ids)
        .bind(mark.max_event_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rooms.into_iter().map(|(room,)| room).collect())
    }

    /// Every gap row for `account_id`, newest first: the newest rows cover the
    /// history a user is most likely to read.
    pub async fn list_timeline_gaps(
        &self,
        account_id: Uuid,
    ) -> Result<Vec<TimelineGap>, StoreError> {
        let sql = format!(
            "SELECT {GAP_COLUMNS} FROM room_timeline_gaps WHERE account_id = $1 ORDER BY id DESC"
        );
        let rows = sqlx_core::query_as::query_as::<Postgres, TimelineGap>(&sql)
            .bind(account_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// Run the first-window test on an unchecked row (ADR 0140 decision 1) and
    /// act on it: a first window, or a batch none of whose events was stored
    /// within `wait`, is deleted; a hole is marked checked and its boundary
    /// resolved. A row still awaiting its batch is left as it is.
    ///
    /// An expired row's hole has no boundary to page to, so the same statement
    /// that deletes it queues the room's verification walk again (#1170): the
    /// walk reads the room from its live end and finds the hole.
    ///
    /// A row is a first window when the room has no stored event outside the
    /// batch whose `received_at` is earlier than the earliest `received_at`
    /// among the batch's stored events. Only an earlier sync response, or
    /// backfill, can have stored such an event, so once a batch event is stored
    /// the answer no longer changes, however long the row waits.
    pub async fn check_timeline_gap(
        &self,
        gap_id: i64,
        wait: std::time::Duration,
    ) -> Result<GapCheck, StoreError> {
        let row: Option<CheckRow> = sqlx_core::query_as::query_as(
            "WITH g AS ( \
                     SELECT id, account_id, room_id, from_token, batch_event_ids, detected_at \
                     FROM room_timeline_gaps WHERE id = $1 \
                 ), b AS ( \
                     SELECT min(e.received_at) AS batch_min \
                     FROM g JOIN events e \
                       ON e.account_id = g.account_id AND e.event_id = ANY(g.batch_event_ids) \
                 ) \
                 SELECT b.batch_min, \
                        g.detected_at < now() - make_interval(secs => $2) AS expired, \
                        b.batch_min IS NOT NULL AND NOT EXISTS ( \
                            SELECT 1 FROM events e \
                            WHERE e.account_id = g.account_id AND e.room_id = g.room_id \
                              AND e.received_at < b.batch_min \
                              AND NOT (e.event_id = ANY(g.batch_event_ids)) \
                        ) AS first_window, \
                        g.from_token \
                 FROM g CROSS JOIN b",
        )
        .bind(gap_id)
        .bind(wait.as_secs_f64())
        .fetch_optional(&self.pool)
        .await?;
        let Some((batch_min, expired, first_window, from_token)) = row else {
            return Ok(GapCheck::Gone);
        };
        let verdict = match batch_min {
            None if expired => GapCheck::Expired { from_token },
            None => return Ok(GapCheck::AwaitingBatch),
            Some(_) if first_window => GapCheck::FirstWindow,
            Some(batch_min) => {
                sqlx_core::query::query(
                    "UPDATE room_timeline_gaps \
                     SET batch_checked = true, stored_before = COALESCE(stored_before, $2) \
                     WHERE id = $1",
                )
                .bind(gap_id)
                .bind(batch_min)
                .execute(&self.pool)
                .await?;
                return Ok(GapCheck::Hole);
            }
        };
        if matches!(verdict, GapCheck::Expired { .. }) {
            sqlx_core::query::query(&crate::history_verify::requeue_on_gap_delete_sql())
                .bind(gap_id)
                .execute(&self.pool)
                .await?;
        } else {
            self.delete_timeline_gap(gap_id).await?;
        }
        Ok(verdict)
    }

    /// Look up a page's event ids in `events` before the page is written: how
    /// many are already stored, and whether the page reaches history stored
    /// before the hole (ADR 0140 decision 2).
    pub async fn lookup_gap_page(
        &self,
        account_id: Uuid,
        gap: &TimelineGap,
        page_event_ids: &[String],
    ) -> Result<GapPageLookup, StoreError> {
        let (already_stored, reaches_stored_history): (i64, bool) = sqlx_core::query_as::query_as(
            "SELECT count(*), \
                    coalesce(bool_or(received_at < $3 OR event_id = ANY($4)), false) \
             FROM events WHERE account_id = $1 AND event_id = ANY($2)",
        )
        .bind(account_id)
        .bind(page_event_ids)
        .bind(gap.stored_before)
        .bind(&gap.stop_event_ids)
        .fetch_one(&self.pool)
        .await?;
        Ok(GapPageLookup {
            already_stored,
            reaches_stored_history,
        })
    }

    /// Save progress after a page that did not close the row: resume from
    /// `end`, count the page and what it inserted, and clear any failure
    /// backoff or parking.
    pub async fn save_timeline_gap_page(
        &self,
        gap_id: i64,
        end: &str,
        inserted: i64,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query(
            "UPDATE room_timeline_gaps SET \
                 from_token = $2, \
                 pages_fetched = pages_fetched + 1, \
                 events_recovered = events_recovered + $3, \
                 consecutive_failures = 0, \
                 last_failure = NULL, \
                 retry_at = NULL, \
                 parked = false \
             WHERE id = $1",
        )
        .bind(gap_id)
        .bind(end)
        .bind(inserted)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete a row — closed, dropped as a first window, or expired — and
    /// return its totals, or `None` if it was already gone.
    pub async fn delete_timeline_gap(&self, gap_id: i64) -> Result<Option<GapTotals>, StoreError> {
        let row: Option<(i32, i64)> = sqlx_core::query_as::query_as(
            "DELETE FROM room_timeline_gaps WHERE id = $1 \
             RETURNING pages_fetched, events_recovered",
        )
        .bind(gap_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(pages_fetched, events_recovered)| GapTotals {
            pages_fetched,
            events_recovered,
        }))
    }

    /// Record a failed page against a gap row (ADR 0121). With
    /// [`clear_token`](RoomBackfillFailure::clear_token) — the homeserver
    /// rejected the saved token — the row restarts from the live end
    /// (ADR 0140 decision 2): its origin becomes `token_rejected` and its
    /// boundary the earliest `received_at` among the batch's stored events, so
    /// the walk passes through the batch and everything stored after it and
    /// stops at the history before the hole. The boundary only ever moves
    /// earlier: if batch events were purged since it was resolved, the
    /// remaining ones' minimum could be later, and the walk would stop early
    /// on events stored inside the hole. A lag probe has no batch and keeps its
    /// boundary, which already means the same.
    ///
    /// `recovered` is how many of the failed page's events were stored all
    /// the same: the retry finds them stored and cannot count them.
    pub async fn record_timeline_gap_failure(
        &self,
        gap_id: i64,
        failure: RoomBackfillFailure<'_>,
        recovered: i64,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query(
            "UPDATE room_timeline_gaps g SET \
                 consecutive_failures = g.consecutive_failures + 1, \
                 last_failure = $2, \
                 retry_at = now() + make_interval(secs => $3), \
                 parked = $4, \
                 origin = CASE WHEN $5 THEN 'token_rejected' ELSE g.origin END, \
                 from_token = CASE WHEN $5 THEN NULL ELSE g.from_token END, \
                 stored_before = CASE WHEN $5 THEN LEAST( \
                     (SELECT min(e.received_at) FROM events e \
                      WHERE e.account_id = g.account_id \
                        AND e.event_id = ANY(g.batch_event_ids)), \
                     g.stored_before) ELSE g.stored_before END, \
                 events_recovered = g.events_recovered + $6 \
             WHERE g.id = $1",
        )
        .bind(gap_id)
        .bind(failure.kind)
        .bind(failure.retry_after.as_secs_f64())
        .bind(failure.parked)
        .bind(failure.clear_token)
        .bind(recovered)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Gap-row counts across every account, for `GET /v1/status`. On the
    /// isolated status pool, like [`backfill_progress`](Self::backfill_progress).
    pub async fn history_gap_counts(&self) -> Result<HistoryGapCounts, StoreError> {
        let (open, parked, awaiting_batch): (i64, i64, i64) = sqlx_core::query_as::query_as(
            "SELECT count(*), \
                    count(*) FILTER (WHERE g.parked), \
                    count(*) FILTER (WHERE NOT g.batch_checked AND NOT EXISTS ( \
                        SELECT 1 FROM events e \
                        WHERE e.account_id = g.account_id \
                          AND e.event_id = ANY(g.batch_event_ids))) \
             FROM room_timeline_gaps g",
        )
        .fetch_one(&self.status_pool)
        .await?;
        Ok(HistoryGapCounts {
            open,
            parked,
            awaiting_batch,
        })
    }
}
