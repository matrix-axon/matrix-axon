//! Filling the holes that limited syncs leave in a room's history (ADR 0140,
//! #1125).
//!
//! When more than `sync.timeline_limit` events reach a room between two
//! sliding-sync responses, the homeserver returns only the newest ones, with
//! `limited: true` and a `prev_batch` token pointing just before them. Ordinary
//! backfill never reaches what lies between those events and what Axon already
//! had, because it only pages behind the oldest stored event.
//!
//! Two halves, joined by the durable `room_timeline_gaps` table:
//!
//! * [`GapWatcher`] reads the SDK's room-update broadcast, which carries each
//!   room's `limited` flag and `prev_batch`, and records a row for every hole.
//!   If the broadcast lags, it cannot tell which lost responses were limited,
//!   so it records a probe for every room that stored events during the lag
//!   (decision 3).
//! * [`Filler`] runs inside the backfill task, ahead of ordinary backfill, and
//!   pages each row backward until a page holds an event stored before the
//!   hole (decision 2). It runs whatever `sync.backfill_enabled` says.
//!
//! The watcher is not a task of its own. The live ingestion handler drains it
//! ([`GapRecorder::drain`]) before it stores an event. The SDK sends a
//! response's broadcast before it runs that response's handlers, awaits the
//! handlers, and only then saves its sliding-sync position. So a hole's row is
//! committed before any of the response's events are stored, and before a
//! restart could resume past the response. A row the database refuses is kept
//! and retried with the same room, token and batch.
//!
//! Every boundary is a stored row's `events.received_at`, which
//! `ON CONFLICT DO NOTHING` never changes; nothing here reads the application
//! clock or orders by `origin_ts`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axon_store::{GapCheck, GapOrigin, LagMark, Store, TimelineGap};
use chrono::{DateTime, Utc};
use matrix_sdk::deserialized_responses::TimelineEvent;
use matrix_sdk::ruma::api::client::message::get_message_events;
use matrix_sdk::{Client, RoomState};
use matrix_sdk_base::sync::RoomUpdates;
use tokio::sync::{broadcast, Mutex, Notify};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    disk_ok, failure_record, fetch_page, write_page, BackfillHealth, BackfillParams, FetchError,
    Page, PageFailure, Written,
};
use crate::engine::PersistContext;
use crate::homeserver_gate::HomeserverGate;

/// How long a limited row may wait for any of its batch to be stored before it
/// is dropped unpaged (ADR 0140 decision 1). The response's own handlers store
/// the batch moments after the row is recorded, so a row still waiting after
/// this has a batch that will never be stored.
const BATCH_WAIT: Duration = Duration::from_secs(10 * 60);

/// How soon the backfill task re-checks a row that awaits its batch, when it
/// would otherwise idle for longer.
pub(super) const AWAITING_RECHECK: Duration = Duration::from_secs(2);

/// How many recent updates with timeline events the watcher remembers, to
/// place the boundary of a lag (ADR 0140 decision 3). Falling back to an older
/// one only makes a probe walk further, never wrongly.
const RECENT_UPDATES: usize = 8;

/// How long the handler that drained a hole keeps retrying its insert, so a
/// database blip or a lock held for a while does not leave the row in memory
/// only. The SDK cannot save its position past the response meanwhile. It is
/// several of `database.timeouts.lock_secs` (3 s by default), so a lock held
/// elsewhere for most of it is outlasted.
const INSERT_BUDGET: Duration = Duration::from_secs(30);

/// The wait before the second insert attempt, doubled for each later one up
/// to [`INSERT_RETRY_MAX`].
const INSERT_RETRY: Duration = Duration::from_millis(100);

const INSERT_RETRY_MAX: Duration = Duration::from_secs(5);

/// The first delay before a refused insert or probe is retried from memory,
/// doubled for each further refusal, up to [`PENDING_RETRY_MAX`].
const PENDING_RETRY: Duration = Duration::from_secs(1);

const PENDING_RETRY_MAX: Duration = Duration::from_secs(30);

/// How often the account's drain task empties the broadcast between live
/// events, so responses that carry no timeline events do not pile up into a
/// lag, and work the database refused is retried.
const DRAIN_INTERVAL: Duration = Duration::from_secs(1);

/// Shared between the watcher, which records rows, and the backfill task, which
/// pages them: whether a row was recorded since the task last looked, and a
/// wake-up for the task's idle wait.
#[derive(Default)]
pub(crate) struct GapSignal {
    recorded: AtomicBool,
    notify: Notify,
}

impl GapSignal {
    /// A row was recorded.
    fn mark(&self) {
        self.recorded.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    /// Whether a row was recorded since the last call.
    pub(super) fn take(&self) -> bool {
        self.recorded.swap(false, Ordering::AcqRel)
    }

    /// Resolves when a row is recorded (or was, since the last wait).
    pub(super) async fn recorded(&self) {
        self.notify.notified().await;
    }
}

/// The joined rooms' ids, read only when a lag is probed. The engine passes
/// [`joined_room_ids`]; tests pass a fixed list. A parameter rather than a
/// field: the recorder lives in the client's handler context, so holding a
/// client in it would keep every replaced client alive.
pub(crate) type JoinedRooms<'a> = &'a (dyn Fn() -> Vec<String> + Send + Sync);

/// The SDK's joined rooms, as [`JoinedRooms`] reads them.
pub(crate) fn joined_room_ids(client: &Client) -> Vec<String> {
    client
        .joined_rooms()
        .iter()
        .map(|room| room.room_id().to_string())
        .collect()
}

/// A limited response's hole whose insert the database refused, kept exactly
/// as the response described it until a retry commits it.
#[derive(Debug, Clone)]
struct PendingHole {
    room_id: String,
    prev_batch: Option<String>,
    batch: Vec<String>,
}

/// Work the database refused, retried from memory with a growing delay.
#[derive(Default)]
struct Pending {
    holes: Vec<PendingHole>,
    /// A lag boundary whose probes are not recorded yet.
    probe: Option<LagMark>,
    /// Consecutive refused retries, for the delay.
    refusals: u32,
    retry_at: Option<Instant>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.holes.is_empty() && self.probe.is_none()
    }

    fn due(&self) -> bool {
        !self.is_empty() && self.retry_at.is_none_or(|at| Instant::now() >= at)
    }

    fn refused(&mut self) {
        self.refusals = self.refusals.saturating_add(1);
        let delay = PENDING_RETRY
            .saturating_mul(1 << self.refusals.saturating_sub(1).min(5))
            .min(PENDING_RETRY_MAX);
        self.retry_at = Some(Instant::now() + delay);
    }

    fn settled(&mut self) {
        if self.is_empty() {
            self.refusals = 0;
            self.retry_at = None;
        }
    }
}

/// Records a row for every hole a limited response leaves, and probes for the
/// holes a lagged broadcast hid (ADR 0140 decisions 1 and 3).
pub(crate) struct GapWatcher {
    account_id: Uuid,
    store: Store,
    timeline_limit: u32,
    signal: Arc<GapSignal>,
    /// The database clock and event-id sequence when the watcher subscribed,
    /// before the sync service started: before any event a response stored.
    /// The lag boundary of last resort.
    subscribed: LagMark,
    /// The event ids of the last few updates that carried timeline events,
    /// newest first.
    recent: VecDeque<Vec<String>>,
    pending: Pending,
    /// [`INSERT_BUDGET`], shortened by tests.
    insert_budget: Duration,
}

impl GapWatcher {
    /// A watcher for one account run. Call it after subscribing to the
    /// broadcast and before the sync service starts, so the clock it reads is
    /// earlier than any event a response stores.
    pub(crate) async fn new(
        account_id: Uuid,
        store: Store,
        timeline_limit: u32,
        signal: Arc<GapSignal>,
    ) -> Self {
        let subscribed = match store.lag_mark_now().await {
            Ok(mark) => mark,
            Err(err) => {
                // Any earlier mark is safe; it only makes a lag probe walk
                // further, down to the room's first window at most.
                tracing::warn!(
                    account_id = %account_id,
                    reason = err.diagnostic_reason(),
                    "gap watcher could not read the database clock; a lag before the first update will probe each room down to its first window"
                );
                LagMark {
                    received_at: DateTime::<Utc>::UNIX_EPOCH,
                    max_event_id: 0,
                }
            }
        };
        Self {
            account_id,
            store,
            timeline_limit,
            signal,
            subscribed,
            recent: VecDeque::with_capacity(RECENT_UPDATES),
            pending: Pending::default(),
            insert_budget: INSERT_BUDGET,
        }
    }

    /// The same watcher with a shorter insert budget, so tests need not wait
    /// for the real one to run out.
    #[cfg(test)]
    pub(crate) fn with_insert_budget(mut self, budget: Duration) -> Self {
        self.insert_budget = budget;
        self
    }

    /// Record the holes in one response's joined rooms.
    pub(crate) async fn observe(&mut self, updates: &RoomUpdates) {
        let mut update_ids = Vec::new();
        for (room_id, update) in &updates.joined {
            let timeline = &update.timeline;
            let batch_ids = event_ids(&timeline.events);
            if timeline.limited {
                // The oldest events come first, and they carry the batch's
                // earliest storage time, which is all the first-window test
                // reads; a server that overfills the batch is cut to size.
                let limit = (self.timeline_limit.max(1) as usize).min(batch_ids.len());
                let hole = PendingHole {
                    room_id: room_id.to_string(),
                    prev_batch: timeline.prev_batch.clone(),
                    batch: batch_ids[..limit].to_vec(),
                };
                self.record_limited(hole).await;
            }
            update_ids.extend(batch_ids);
        }
        if !update_ids.is_empty() {
            self.recent.push_front(update_ids);
            self.recent.truncate(RECENT_UPDATES);
        }
    }

    /// Insert a hole's row, retrying briefly while the caller waits. A row the
    /// database still refuses is kept, unchanged, for
    /// [`retry_pending`](Self::retry_pending).
    async fn record_limited(&mut self, hole: PendingHole) {
        // A hole already waiting in memory means the database is refusing
        // these inserts: queue behind it rather than stall the handler for a
        // whole budget per room. Its retry records this one too.
        if !self.pending.holes.is_empty() {
            tracing::error!(
                account_id = %self.account_id,
                room_id = %hole.room_id,
                prev_batch = hole.prev_batch.as_deref().unwrap_or("<none: live end>"),
                "gap table is refusing inserts; keeping this limited sync's hole in memory and retrying"
            );
            self.pending.holes.push(hole);
            return;
        }
        let started = Instant::now();
        let mut delay = INSERT_RETRY;
        let mut attempt = 1u32;
        loop {
            let reason = match self.insert_hole(&hole).await {
                Ok(()) => return,
                Err(reason) => reason,
            };
            if started.elapsed() + delay < self.insert_budget {
                tracing::warn!(account_id = %self.account_id, room_id = %hole.room_id, attempt, reason, "could not record a limited sync's hole; retrying");
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2).min(INSERT_RETRY_MAX);
                attempt += 1;
                continue;
            }
            // Until the retry commits it, a restart forgets this hole: the SDK
            // saves its position once this response's handlers finish.
            tracing::error!(
                account_id = %self.account_id,
                room_id = %hole.room_id,
                prev_batch = hole.prev_batch.as_deref().unwrap_or("<none: live end>"),
                attempts = attempt,
                reason,
                "could not record a limited sync's hole; keeping it in memory and retrying"
            );
            self.pending.holes.push(hole);
            self.pending.refused();
            return;
        }
    }

    async fn insert_hole(&self, hole: &PendingHole) -> Result<(), &'static str> {
        let gap_id = self
            .store
            .record_limited_gap(
                self.account_id,
                &hole.room_id,
                hole.prev_batch.as_deref(),
                &hole.batch,
            )
            .await
            .map_err(|err| err.diagnostic_reason())?;
        tracing::info!(
            account_id = %self.account_id,
            room_id = %hole.room_id,
            gap_id,
            prev_batch = hole.prev_batch.as_deref().unwrap_or("<none: live end>"),
            oldest_event_id = hole.batch.first().map_or("<none>", String::as_str),
            batch_len = hole.batch.len(),
            "limited sync left a hole in the room's history; recorded it for filling"
        );
        self.signal.mark();
        Ok(())
    }

    /// The broadcast dropped `skipped` updates. Probe every joined room that
    /// stored events after the last update this watcher read.
    pub(crate) async fn lagged(&mut self, skipped: u64, joined: JoinedRooms<'_>) {
        let boundary = self.lag_boundary().await;
        // Two pending lags merge into the earlier boundary, which covers both.
        self.pending.probe = Some(
            self.pending
                .probe
                .map_or(boundary, |pending| pending.earliest(boundary)),
        );
        if !self.record_probes(skipped, joined).await {
            self.pending.refused();
        }
    }

    /// The latest `received_at` (and highest id) among the stored events of
    /// the newest remembered update that has any, else the subscription mark
    /// (ADR 0140 decision 3). The SDK finished that update's handlers before
    /// it took any response the broadcast lost, so every event a lost response
    /// stored is later, however long this watcher stalled.
    async fn lag_boundary(&self) -> LagMark {
        for ids in &self.recent {
            match self.store.lag_mark(self.account_id, ids).await {
                Ok(Some(mark)) => return mark,
                Ok(None) => continue,
                Err(err) => {
                    tracing::warn!(
                        account_id = %self.account_id,
                        reason = err.diagnostic_reason(),
                        "could not read the lag boundary; using the watcher's subscription time"
                    );
                    break;
                }
            }
        }
        self.subscribed
    }

    /// Record the pending lag's probes. Returns `false` if the database
    /// refused them; they stay pending.
    async fn record_probes(&mut self, skipped: u64, joined: JoinedRooms<'_>) -> bool {
        let Some(boundary) = self.pending.probe else {
            return true;
        };
        let joined = joined();
        match self
            .store
            .record_lag_probes(self.account_id, boundary, &joined)
            .await
        {
            Ok(rooms) => {
                self.pending.probe = None;
                tracing::warn!(
                    account_id = %self.account_id,
                    boundary = %boundary.received_at,
                    above_event_id = boundary.max_event_id,
                    skipped,
                    rooms_probed = rooms.len(),
                    "room-update broadcast lagged; probing the rooms that stored events during the lag"
                );
                for room_id in &rooms {
                    tracing::debug!(account_id = %self.account_id, room_id = %room_id, boundary = %boundary.received_at, "recorded a lag probe");
                }
                if !rooms.is_empty() {
                    self.signal.mark();
                }
                true
            }
            Err(err) => {
                tracing::warn!(
                    account_id = %self.account_id,
                    boundary = %boundary.received_at,
                    reason = err.diagnostic_reason(),
                    "could not record lag probes; retrying"
                );
                false
            }
        }
    }

    /// Retry the holes and probes the database refused, once their delay has
    /// passed.
    pub(crate) async fn retry_pending(&mut self, joined: JoinedRooms<'_>) {
        if !self.pending.due() {
            return;
        }
        let mut refused = false;
        let holes = std::mem::take(&mut self.pending.holes);
        for hole in holes {
            if refused {
                self.pending.holes.push(hole);
                continue;
            }
            if let Err(reason) = self.insert_hole(&hole).await {
                tracing::warn!(account_id = %self.account_id, room_id = %hole.room_id, reason, "could not record a kept hole; retrying");
                refused = true;
                self.pending.holes.push(hole);
            }
        }
        if !refused && !self.record_probes(0, joined).await {
            refused = true;
        }
        if refused {
            self.pending.refused();
        } else {
            self.pending.settled();
        }
    }
}

/// The event ids of a sync batch, in its order (oldest first).
fn event_ids(events: &[TimelineEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| event.event_id().map(ToString::to_string))
        .collect()
}

/// One account run's gap watcher with its own subscription to the
/// room-update broadcast, drained in-line by the live ingestion handler.
pub(crate) struct GapRecorder {
    state: Mutex<(broadcast::Receiver<RoomUpdates>, GapWatcher)>,
}

impl GapRecorder {
    /// Subscribe before the sync service starts, so the first response is not
    /// missed.
    pub(crate) fn new(updates: broadcast::Receiver<RoomUpdates>, watcher: GapWatcher) -> Self {
        Self {
            state: Mutex::new((updates, watcher)),
        }
    }

    /// Record the holes of every response broadcast so far. The live
    /// ingestion handler calls this before it stores an event, so a
    /// response's holes are committed before its events are, and before the
    /// SDK saves the position past it. Holding the lock across the inserts
    /// makes a concurrent caller wait for them too.
    pub(crate) async fn drain(&self, joined: JoinedRooms<'_>) {
        let mut state = self.state.lock().await;
        let (updates, watcher) = &mut *state;
        watcher.retry_pending(joined).await;
        loop {
            match updates.try_recv() {
                Ok(batch) => watcher.observe(&batch).await,
                Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                    watcher.lagged(skipped, joined).await;
                }
                Err(
                    broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed,
                ) => {
                    return;
                }
            }
        }
    }
}

/// Drain `recorder` every [`DRAIN_INTERVAL`] until `cancel` fires: between
/// live events, responses that carry none still go through it, so they do not
/// fill the broadcast into a lag, and refused work is retried.
pub(crate) async fn watch(recorder: Arc<GapRecorder>, client: Client, cancel: CancellationToken) {
    let joined = move || joined_room_ids(&client);
    let mut tick = tokio::time::interval(DRAIN_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => recorder.drain(&joined).await,
        }
    }
}

/// What one pass over an account's gap rows did.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct GapPass {
    /// At least one page was fetched and written.
    pub(super) progressed: bool,
    /// A row is still waiting for its batch to be stored.
    pub(super) awaiting_batch: bool,
    /// A joined room's row is waiting out a failure backoff.
    pub(super) backing_off: bool,
}

impl GapPass {
    /// Rows the next pass should look at again soon.
    pub(super) fn pending(self) -> bool {
        self.awaiting_batch || self.backing_off
    }
}

/// The result of paging one gap row once.
enum GapPage {
    /// Written, and the row continues from the page's `end`.
    Paged,
    /// Written, and the row closed: the page reached stored history or the
    /// room's start.
    Closed,
    /// The homeserver did not serve the page, or the store failed while it
    /// was written or the row updated. Recorded against the row, so it backs
    /// off rather than being fetched again every sweep.
    Failed(PageFailure),
    /// Cancelled. Nothing is recorded against the row.
    Abandoned,
}

/// Pages an account's gap rows inside the backfill task (ADR 0140 decision 2).
pub(super) struct Filler<'a> {
    pub(super) client: &'a Client,
    pub(super) ctx: &'a PersistContext,
    pub(super) params: &'a BackfillParams,
    pub(super) health: &'a BackfillHealth,
    pub(super) gate: &'a Arc<HomeserverGate>,
    pub(super) cancel: &'a CancellationToken,
}

impl Filler<'_> {
    /// Page every open row until none can make progress: each closed, waiting
    /// for its batch or a backoff, in a room the account has left, or paused
    /// by the disk guard. Newest row first in each round.
    pub(super) async fn fill(&self) -> GapPass {
        let account_id = self.ctx.account_id;
        let mut pass = GapPass::default();
        loop {
            if self.cancel.is_cancelled() {
                return pass;
            }
            let rows = match self.ctx.store.list_timeline_gaps(account_id).await {
                Ok(rows) => rows,
                Err(err) => {
                    tracing::warn!(account_id = %account_id, error = %err, "gap fill: list rows failed; retrying next sweep");
                    return pass;
                }
            };
            pass.awaiting_batch = false;
            pass.backing_off = false;
            let mut advanced = false;
            for gap in rows {
                if self.cancel.is_cancelled() {
                    return pass;
                }
                // A room the account has left waits: its rows go if the room is
                // purged, and resume if it is rejoined first (decision 6).
                let Some(room) = matrix_sdk::ruma::RoomId::parse(&gap.room_id)
                    .ok()
                    .and_then(|room_id| self.client.get_room(&room_id))
                    .filter(|room| room.state() == RoomState::Joined)
                else {
                    continue;
                };
                if !gap.retry_due {
                    pass.backing_off |= !gap.parked;
                    continue;
                }
                if !gap.batch_checked {
                    advanced |= self.check(&gap, &mut pass).await;
                    continue;
                }
                if !disk_ok(self.params, self.health, account_id) {
                    return pass;
                }
                match self.page(&room, &gap).await {
                    GapPage::Paged | GapPage::Closed => {
                        advanced = true;
                        pass.progressed = true;
                        tokio::select! {
                            _ = self.cancel.cancelled() => return pass,
                            _ = tokio::time::sleep(self.params.throttle) => {}
                        }
                    }
                    GapPage::Failed(failure) => self.record_failure(&gap, failure).await,
                    GapPage::Abandoned => {}
                }
            }
            if !advanced {
                return pass;
            }
        }
    }

    /// Run the first-window test on an unchecked row. Returns whether the row
    /// became pageable, so the caller takes another round.
    async fn check(&self, gap: &TimelineGap, pass: &mut GapPass) -> bool {
        let account_id = self.ctx.account_id;
        match self.ctx.store.check_timeline_gap(gap.id, BATCH_WAIT).await {
            Ok(GapCheck::Hole) => true,
            Ok(GapCheck::AwaitingBatch) => {
                pass.awaiting_batch = true;
                false
            }
            Ok(GapCheck::FirstWindow) => {
                tracing::debug!(
                    account_id = %account_id,
                    room_id = %gap.room_id,
                    gap_id = gap.id,
                    "limited sync was the room's first window, not a hole; dropped unpaged"
                );
                false
            }
            Ok(GapCheck::Expired { from_token }) => {
                tracing::warn!(
                    account_id = %account_id,
                    room_id = %gap.room_id,
                    gap_id = gap.id,
                    from_token = from_token.as_deref().unwrap_or("<none: live end>"),
                    "no event of the limited batch was stored within 10 minutes; dropped the row unpaged"
                );
                false
            }
            Ok(GapCheck::Gone) => false,
            Err(err) => {
                tracing::warn!(account_id = %account_id, room_id = %gap.room_id, gap_id = gap.id, error = %err, "gap fill: first-window check failed; retrying next sweep");
                false
            }
        }
    }

    /// Page one row one step older, write the page, and close or advance the
    /// row (ADR 0140 decision 2).
    async fn page(&self, room: &matrix_sdk::Room, gap: &TimelineGap) -> GapPage {
        let account_id = self.ctx.account_id;
        let store = &self.ctx.store;
        let mut request = get_message_events::v3::Request::backward(room.room_id().to_owned());
        request.limit = self.params.page_size.into();
        let had_token = gap.from_token.is_some();
        request.from = gap.from_token.clone();

        let Page {
            events,
            end,
            key_arrivals,
            ..
        } = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return GapPage::Abandoned,
            result = fetch_page(self.gate, room, request, self.params.page_timeout) => match result {
                Ok(page) => page,
                Err(FetchError::Homeserver(err)) => {
                    let failure = PageFailure::of(&err, had_token);
                    tracing::warn!(account_id = %account_id, room_id = %gap.room_id, gap_id = gap.id, error = %err, failure = failure.as_str(), "gap fill: /messages page failed");
                    return GapPage::Failed(failure);
                }
                Err(FetchError::TimedOut) => {
                    tracing::warn!(account_id = %account_id, room_id = %gap.room_id, gap_id = gap.id, timeout_secs = self.params.page_timeout.as_secs(), "gap fill: /messages page timed out");
                    return GapPage::Failed(PageFailure::TimedOut);
                }
            },
        };

        // Looked up before the page is written, so the page's own writes are
        // not mistaken for history stored before the hole.
        let ids = event_ids(&events);
        let lookup = match store.lookup_gap_page(account_id, gap, &ids).await {
            Ok(lookup) => lookup,
            Err(err) => {
                tracing::warn!(account_id = %account_id, room_id = %gap.room_id, gap_id = gap.id, error = %err, "gap fill: page lookup failed");
                return GapPage::Failed(PageFailure::StoreWrite);
            }
        };
        // Written in full even when it closes the row, so the hole's events on
        // it are kept; the already-stored ones are no-ops.
        let written = write_page(self.ctx, room, &events, key_arrivals, self.cancel).await;
        let (inserted, failed) = match written {
            Written::Abandoned => return GapPage::Abandoned,
            Written::Complete { inserted } => (inserted, 0),
            Written::StoreFailed { failed, inserted } => (inserted, failed),
        };
        self.health
            .add_gap_events_recovered(u64::try_from(inserted).unwrap_or(u64::MAX));
        let inserted = i64::try_from(inserted).unwrap_or(i64::MAX);
        tracing::debug!(
            account_id = %account_id,
            room_id = %gap.room_id,
            gap_id = gap.id,
            inserted,
            already_stored = lookup.already_stored,
            "gap fill: wrote a page"
        );
        // An event the store refused is still missing: neither advance nor
        // close the row, so the page is fetched again after a backoff. Its
        // other events are no-ops then, and do not move the boundary.
        if failed > 0 {
            tracing::warn!(account_id = %account_id, room_id = %gap.room_id, gap_id = gap.id, failed, "gap fill: events on the page could not be stored; keeping the row's token to fetch it again");
            return GapPage::Failed(PageFailure::StoreWrite);
        }

        let end = match end {
            Some(end) if !lookup.reaches_stored_history => end,
            _ => {
                let reason = if lookup.reaches_stored_history {
                    "reached history stored before the hole"
                } else {
                    "reached the room's start"
                };
                return match store.delete_timeline_gap(gap.id).await {
                    Ok(totals) => {
                        let totals = totals.unwrap_or(axon_store::GapTotals {
                            pages_fetched: gap.pages_fetched,
                            events_recovered: gap.events_recovered,
                        });
                        let pages_fetched = totals.pages_fetched + 1;
                        let events_recovered = totals.events_recovered + inserted;
                        if events_recovered == 0 {
                            // A room coming back into the sliding-sync window can
                            // be reported limited with nothing missing (ADR 0140,
                            // Consequences). Logged apart from real holes, so
                            // their rate can be measured.
                            tracing::info!(
                                account_id = %account_id,
                                room_id = %gap.room_id,
                                gap_id = gap.id,
                                origin = gap.origin.as_str(),
                                pages_fetched,
                                reason,
                                "limited sync had nothing missing; closed the row"
                            );
                        } else {
                            tracing::info!(
                                account_id = %account_id,
                                room_id = %gap.room_id,
                                gap_id = gap.id,
                                origin = gap.origin.as_str(),
                                pages_fetched,
                                events_recovered,
                                reason,
                                "closed a hole in the room's history"
                            );
                        }
                        GapPage::Closed
                    }
                    Err(err) => {
                        // Paged again after a backoff; the page's writes are
                        // no-ops then.
                        tracing::warn!(account_id = %account_id, room_id = %gap.room_id, gap_id = gap.id, error = %err, "gap fill: closing the row failed");
                        GapPage::Failed(PageFailure::StoreWrite)
                    }
                };
            }
        };
        match store.save_timeline_gap_page(gap.id, &end, inserted).await {
            Ok(()) => GapPage::Paged,
            Err(err) => {
                tracing::warn!(account_id = %account_id, room_id = %gap.room_id, gap_id = gap.id, error = %err, "gap fill: saving progress failed");
                GapPage::Failed(PageFailure::StoreWrite)
            }
        }
    }

    /// Back a failing row off, park it, or restart it from the live end
    /// (ADR 0121, ADR 0140 decision 2). A row whose token was already rejected
    /// once is parked by a second rejection, as a backfill cursor is.
    async fn record_failure(&self, gap: &TimelineGap, failure: PageFailure) {
        let account_id = self.ctx.account_id;
        let prior = gap.consecutive_failures;
        let prior_rejections = i32::from(gap.origin == GapOrigin::TokenRejected);
        // Gap rows keep no separate timeout streak: a row's failures are its
        // streak, since any successful page resets both.
        let record = failure_record(failure, prior, prior_rejections, prior);
        let failures = prior.saturating_add(1);
        let retry_in_secs = record.retry_after.as_secs();
        if record.parked && !gap.parked {
            tracing::warn!(account_id = %account_id, room_id = %gap.room_id, gap_id = gap.id, failure = record.kind, failures, retry_in_secs, "gap fill: parking row after repeated failures");
        } else if record.clear_token {
            tracing::warn!(account_id = %account_id, room_id = %gap.room_id, gap_id = gap.id, "gap fill: homeserver rejected the saved token; restarting the row from the live end");
        } else {
            tracing::info!(account_id = %account_id, room_id = %gap.room_id, gap_id = gap.id, failure = record.kind, failures, retry_in_secs, "gap fill: backing off row");
        }
        if let Err(err) = self
            .ctx
            .store
            .record_timeline_gap_failure(gap.id, record)
            .await
        {
            tracing::warn!(account_id = %account_id, room_id = %gap.room_id, gap_id = gap.id, error = %err, "gap fill: record failure failed");
        }
    }
}

#[cfg(test)]
mod tests;
