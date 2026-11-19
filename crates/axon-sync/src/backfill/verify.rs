//! The one-time pass that re-reads each room's stored history against the
//! homeserver (ADR 0140 decision 4, as amended 2026-11-18, #1125).
//!
//! Holes that limited syncs left before ADR 0140 recorded them have no gap
//! row, and nothing on the client side can locate them. So the backfill task
//! walks each joined room from its live end, storing every page
//! (already-stored events are no-ops), and closes the walk at the first of:
//!
//! * a page holding the room's lowest-`id` event, the first Axon stored there,
//!   below which no limited sync can have left a hole;
//! * [`FLOOR_RUN`] events in a row that were not stored and are older than the
//!   room's floor, the oldest `origin_ts` among its `timeline_limit`
//!   lowest-`id` events: the walk has passed everything Axon stored, and the
//!   homeserver no longer serves the stop event (it purged the room);
//! * the room's start, logged at `WARN`.
//!
//! The walk shares the backfill task with ordinary backfill: after the open
//! gap rows, one verification page alternates with one ordinary page, and
//! verification takes every page while ordinary backfill has none to fetch.
//! A room is not walked while it has an open gap row, which is filling the
//! same newest history, and only joined rooms are walked.

use std::collections::HashSet;
use std::sync::Arc;

use axon_store::{HistoryVerifyRow, VerifyStop, FLOOR_RUN};
use matrix_sdk::deserialized_responses::TimelineEvent;
use matrix_sdk::ruma::api::client::message::get_message_events;
use matrix_sdk::{Client, RoomState};
use tokio_util::sync::CancellationToken;

use super::gaps::event_ids;
use super::{
    disk_ok, failure_record, fetch_page, write_page, BackfillHealth, BackfillParams, FetchError,
    Page, PageFailure, Written,
};
use crate::engine::PersistContext;
use crate::homeserver_gate::HomeserverGate;

/// What one verification step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VerifyStep {
    /// A page was fetched and written (whether or not it closed the walk).
    Paged,
    /// A page failed and was recorded against its room; other rooms may still
    /// be paged.
    Failed,
    /// No room can be walked now: every row is complete, held, backing off,
    /// in a room the account is not in, or the disk guard has paused paging.
    Idle,
    /// Cancelled.
    Abandoned,
}

/// The result of paging one room's walk once.
enum WalkPage {
    /// Written, and the walk continues from the page's `end`.
    Paged,
    /// Written (or nothing to write), and the walk closed.
    Closed,
    /// Recorded against the row, which backs off.
    Failed(PageFailure),
    /// The row is gone: the room was purged meanwhile.
    Gone,
    Abandoned,
}

/// One paged event, as the floor rule reads it.
#[derive(Debug, Clone, Copy)]
struct FloorEvent {
    stored: bool,
    origin_ts: Option<i64>,
}

/// The floor run after a page (ADR 0140 decision 4): it grows by the page's
/// events when none was already stored and every one is older than the floor,
/// and is reset otherwise. A page with no events counts as a full page, so a
/// homeserver that keeps answering with empty pages cannot hold the walk
/// open. A room with no floor never accumulates a run.
fn next_floor_run(run: i32, page: &[FloorEvent], floor_ts: Option<i64>, page_size: u32) -> i32 {
    let Some(floor) = floor_ts else {
        return 0;
    };
    if page.is_empty() {
        return run.saturating_add(i32::try_from(page_size).unwrap_or(i32::MAX));
    }
    let below = page
        .iter()
        .all(|event| !event.stored && event.origin_ts.is_some_and(|ts| ts < floor));
    if below {
        run.saturating_add(i32::try_from(page.len()).unwrap_or(i32::MAX))
    } else {
        0
    }
}

/// A paged event's `origin_server_ts`, if it carries one.
fn origin_ts(event: &TimelineEvent) -> Option<i64> {
    event
        .raw()
        .get_field::<i64>("origin_server_ts")
        .ok()
        .flatten()
}

/// Walks an account's rooms inside the backfill task.
pub(super) struct Verifier<'a> {
    pub(super) client: &'a Client,
    pub(super) ctx: &'a PersistContext,
    pub(super) params: &'a BackfillParams,
    pub(super) health: &'a BackfillHealth,
    pub(super) gate: &'a Arc<HomeserverGate>,
    pub(super) cancel: &'a CancellationToken,
    /// The incomplete rows, most recently active room first, read by
    /// [`refresh`](Self::refresh).
    rows: Vec<HistoryVerifyRow>,
    /// The row [`step`](Self::step) pages next. A walk stays on one room until
    /// it closes or fails.
    next: usize,
    /// Whether the last refresh found incomplete rows, so the pass's end is
    /// logged once.
    had_rows: Option<bool>,
}

impl<'a> Verifier<'a> {
    pub(super) fn new(
        client: &'a Client,
        ctx: &'a PersistContext,
        params: &'a BackfillParams,
        health: &'a BackfillHealth,
        gate: &'a Arc<HomeserverGate>,
        cancel: &'a CancellationToken,
    ) -> Self {
        Self {
            client,
            ctx,
            params,
            health,
            gate,
            cancel,
            rows: Vec::new(),
            next: 0,
            had_rows: None,
        }
    }

    /// Re-read the incomplete rows, and record for status which of them are
    /// in rooms the account is not in. Once per backfill sweep.
    pub(super) async fn refresh(&mut self) {
        self.rows.clear();
        self.next = 0;
        if !self.params.verify_enabled {
            return;
        }
        let account_id = self.ctx.account_id;
        let rows = match self.ctx.store.list_history_verify(account_id).await {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(account_id = %account_id, error = %err, "history verify: list rows failed; retrying next sweep");
                return;
            }
        };
        let joined: HashSet<String> = super::gaps::joined_room_ids(self.client)
            .into_iter()
            .collect();
        let (mut now_joined, mut now_departed) = (Vec::new(), Vec::new());
        for row in &rows {
            match (row.not_joined, joined.contains(&row.room_id)) {
                (true, true) => now_joined.push(row.room_id.clone()),
                (false, false) => now_departed.push(row.room_id.clone()),
                _ => {}
            }
        }
        for (rooms, is_joined) in [(&now_joined, true), (&now_departed, false)] {
            if let Err(err) = self
                .ctx
                .store
                .mark_history_verify_joined(account_id, rooms, is_joined)
                .await
            {
                tracing::warn!(account_id = %account_id, error = %err, "history verify: recording joined rooms failed");
            }
        }
        let pending = rows
            .iter()
            .filter(|row| joined.contains(&row.room_id))
            .count();
        match (self.had_rows, pending > 0) {
            (Some(true), false) => tracing::info!(
                account_id = %account_id,
                rooms_not_joined = rows.len(),
                "history verify: every joined room's stored history has been verified"
            ),
            (None, true) => tracing::info!(
                account_id = %account_id,
                rooms_pending = pending,
                rooms_not_joined = rows.len() - pending,
                "history verify: verifying the stored history of joined rooms"
            ),
            _ => {}
        }
        self.had_rows = Some(pending > 0);
        self.rows = rows;
    }

    /// Page the next walkable room once.
    pub(super) async fn step(&mut self) -> VerifyStep {
        let account_id = self.ctx.account_id;
        while self.next < self.rows.len() {
            if self.cancel.is_cancelled() {
                return VerifyStep::Abandoned;
            }
            let row = &self.rows[self.next];
            if row.held || !row.retry_due {
                self.next += 1;
                continue;
            }
            let Some(room) = matrix_sdk::ruma::RoomId::parse(&row.room_id)
                .ok()
                .and_then(|room_id| self.client.get_room(&room_id))
                .filter(|room| room.state() == RoomState::Joined)
            else {
                self.next += 1;
                continue;
            };
            if !disk_ok(self.params, self.health, account_id) {
                return VerifyStep::Idle;
            }
            return match self.page(&room).await {
                WalkPage::Paged => VerifyStep::Paged,
                WalkPage::Closed => {
                    self.next += 1;
                    VerifyStep::Paged
                }
                WalkPage::Failed(failure) => {
                    self.record_failure(failure).await;
                    self.next += 1;
                    VerifyStep::Failed
                }
                WalkPage::Gone => {
                    self.next += 1;
                    continue;
                }
                WalkPage::Abandoned => VerifyStep::Abandoned,
            };
        }
        VerifyStep::Idle
    }

    /// Page the current row's room one step older, write the page, and close
    /// or advance the walk.
    async fn page(&mut self, room: &matrix_sdk::Room) -> WalkPage {
        let account_id = self.ctx.account_id;
        let store = &self.ctx.store;
        let row = &mut self.rows[self.next];
        let room_id = row.room_id.clone();

        if row.stop_event_ids.is_none() {
            match store
                .start_history_verify(account_id, &room_id, self.params.timeline_limit)
                .await
            {
                Ok(Some(stops)) if stops.stop_event_ids.is_empty() => {
                    // Every event of the room was deleted since it was queued:
                    // there is no stored history to verify.
                    return match store
                        .complete_history_verify(
                            account_id,
                            &room_id,
                            VerifyStop::NoHistory,
                            false,
                            0,
                        )
                        .await
                    {
                        Ok(_) => {
                            tracing::debug!(account_id = %account_id, room_id = %room_id, "history verify: the room has no stored history left; closed the walk");
                            WalkPage::Closed
                        }
                        Err(err) => {
                            tracing::warn!(account_id = %account_id, room_id = %room_id, error = %err, "history verify: closing the walk failed");
                            WalkPage::Failed(PageFailure::StoreWrite)
                        }
                    };
                }
                Ok(Some(stops)) => {
                    tracing::debug!(
                        account_id = %account_id,
                        room_id = %room_id,
                        stop_event_id = %stops.stop_event_ids[0],
                        floor_ts = stops.floor_ts,
                        "history verify: starting the room's walk from its live end"
                    );
                    row.stop_event_ids = Some(stops.stop_event_ids);
                    row.floor_ts = stops.floor_ts;
                    row.floor_run = 0;
                }
                Ok(None) => return WalkPage::Gone,
                Err(err) => {
                    tracing::warn!(account_id = %account_id, room_id = %room_id, error = %err, "history verify: reading the stop points failed");
                    return WalkPage::Failed(PageFailure::StoreWrite);
                }
            }
        }
        let stop_event_ids = row.stop_event_ids.clone().unwrap_or_default();

        let mut request = get_message_events::v3::Request::backward(room.room_id().to_owned());
        request.limit = self.params.page_size.into();
        let had_token = row.from_token.is_some();
        request.from = row.from_token.clone();

        let Page {
            events,
            end,
            key_arrivals,
            ..
        } = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return WalkPage::Abandoned,
            result = fetch_page(self.gate, room, request, self.params.page_timeout) => match result {
                Ok(page) => page,
                Err(FetchError::Homeserver(err)) => {
                    let failure = PageFailure::of(&err, had_token);
                    tracing::warn!(account_id = %account_id, room_id = %room_id, error = %err, failure = failure.as_str(), "history verify: /messages page failed");
                    return WalkPage::Failed(failure);
                }
                Err(FetchError::TimedOut) => {
                    tracing::warn!(account_id = %account_id, room_id = %room_id, timeout_secs = self.params.page_timeout.as_secs(), "history verify: /messages page timed out");
                    return WalkPage::Failed(PageFailure::TimedOut);
                }
            },
        };

        // Looked up before the page is written, so the page's own writes are
        // not counted as stored history.
        let ids = event_ids(&events);
        let lookup = match store
            .lookup_verify_page(account_id, &ids, &stop_event_ids)
            .await
        {
            Ok(lookup) => lookup,
            Err(err) => {
                tracing::warn!(account_id = %account_id, room_id = %room_id, error = %err, "history verify: page lookup failed");
                return WalkPage::Failed(PageFailure::StoreWrite);
            }
        };
        let written = write_page(self.ctx, room, &events, key_arrivals, self.cancel).await;
        let (inserted, failed) = match written {
            Written::Abandoned => return WalkPage::Abandoned,
            Written::Complete { inserted } => (inserted, 0),
            Written::StoreFailed { failed, inserted } => (inserted, failed),
        };
        let inserted = i64::try_from(inserted).unwrap_or(i64::MAX);
        tracing::debug!(
            account_id = %account_id,
            room_id = %room_id,
            inserted,
            already_stored = lookup.stored.len(),
            "history verify: wrote a page"
        );
        // An event the store refused is still missing: neither advance nor
        // close the walk, so the page is fetched again after a backoff.
        if failed > 0 {
            tracing::warn!(account_id = %account_id, room_id = %room_id, failed, "history verify: events on the page could not be stored; keeping the walk's token to fetch it again");
            return WalkPage::Failed(PageFailure::StoreWrite);
        }

        let floor_events: Vec<FloorEvent> = events
            .iter()
            .map(|event| FloorEvent {
                stored: event
                    .event_id()
                    .is_some_and(|id| lookup.stored.contains(id.as_str())),
                origin_ts: origin_ts(event),
            })
            .collect();
        let floor_run = next_floor_run(
            row.floor_run,
            &floor_events,
            row.floor_ts,
            self.params.page_size,
        );
        let stop = if lookup.reaches_stop {
            Some(VerifyStop::FirstStoredEvent)
        } else if end.is_none() {
            Some(VerifyStop::RoomStart)
        } else if floor_run >= FLOOR_RUN {
            Some(VerifyStop::Floor)
        } else {
            None
        };

        let (Some(end), None) = (end, stop) else {
            let stop = stop.unwrap_or(VerifyStop::RoomStart);
            return match store
                .complete_history_verify(account_id, &room_id, stop, true, inserted)
                .await
            {
                Ok(totals) => {
                    let totals = totals.unwrap_or(axon_store::VerifyTotals {
                        pages_fetched: row.pages_fetched + 1,
                        events_recovered: row.events_recovered + inserted,
                    });
                    log_closed(account_id, &room_id, stop, totals);
                    WalkPage::Closed
                }
                Err(err) => {
                    // Paged again after a backoff; the page's writes are
                    // no-ops then.
                    tracing::warn!(account_id = %account_id, room_id = %room_id, error = %err, "history verify: closing the walk failed");
                    WalkPage::Failed(PageFailure::StoreWrite)
                }
            };
        };
        match store
            .save_history_verify_page(account_id, &room_id, &end, inserted, floor_run)
            .await
        {
            Ok(()) => {
                row.from_token = Some(end);
                row.floor_run = floor_run;
                row.pages_fetched += 1;
                row.events_recovered += inserted;
                row.consecutive_failures = 0;
                WalkPage::Paged
            }
            Err(err) => {
                tracing::warn!(account_id = %account_id, room_id = %room_id, error = %err, "history verify: saving progress failed");
                WalkPage::Failed(PageFailure::StoreWrite)
            }
        }
    }

    /// Back the current row off, park it, or restart it from the live end
    /// (ADR 0121). Rows keep no separate timeout streak: a row's failures are
    /// its streak, since any successful page resets both.
    async fn record_failure(&self, failure: PageFailure) {
        let account_id = self.ctx.account_id;
        let row = &self.rows[self.next];
        let prior = row.consecutive_failures;
        let record = failure_record(failure, prior, row.token_rejections, prior);
        let failures = prior.saturating_add(1);
        let retry_in_secs = record.retry_after.as_secs();
        if record.parked && !row.parked {
            tracing::warn!(account_id = %account_id, room_id = %row.room_id, failure = record.kind, failures, retry_in_secs, "history verify: parking room after repeated failures");
        } else if record.clear_token {
            tracing::warn!(account_id = %account_id, room_id = %row.room_id, "history verify: homeserver rejected the saved token; restarting the walk from the live end");
        } else {
            tracing::info!(account_id = %account_id, room_id = %row.room_id, failure = record.kind, failures, retry_in_secs, "history verify: backing off room");
        }
        if let Err(err) = self
            .ctx
            .store
            .record_history_verify_failure(account_id, &row.room_id, record)
            .await
        {
            tracing::warn!(account_id = %account_id, room_id = %row.room_id, error = %err, "history verify: record failure failed");
        }
    }
}

/// Log a closed walk. A walk that stored missing events is evidence of past
/// loss and logs at `WARN`, once per room; so does a walk that reached the
/// room's start, which can store history older than anything Axon held.
fn log_closed(
    account_id: uuid::Uuid,
    room_id: &str,
    stop: VerifyStop,
    totals: axon_store::VerifyTotals,
) {
    let axon_store::VerifyTotals {
        pages_fetched,
        events_recovered,
    } = totals;
    let reason = stop.as_str();
    if stop == VerifyStop::RoomStart {
        tracing::warn!(
            account_id = %account_id,
            room_id = %room_id,
            pages_fetched,
            events_recovered,
            "history verify: walked to the room's start without meeting its first stored event"
        );
    } else if events_recovered > 0 {
        tracing::warn!(
            account_id = %account_id,
            room_id = %room_id,
            pages_fetched,
            events_recovered,
            reason,
            "history verify: stored history that was missing from the room"
        );
    } else if stop == VerifyStop::Floor {
        tracing::info!(
            account_id = %account_id,
            room_id = %room_id,
            pages_fetched,
            reason,
            "history verify: passed below the room's stored history; the homeserver no longer serves its first stored event"
        );
    } else {
        tracing::debug!(
            account_id = %account_id,
            room_id = %room_id,
            pages_fetched,
            reason,
            "history verify: the room's stored history is complete"
        );
    }
}

#[cfg(test)]
mod tests;
