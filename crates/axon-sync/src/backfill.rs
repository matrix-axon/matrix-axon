//! History backfill engine (M10). See ADR 0043.
//!
//! A continuous, throttled, low-priority background task — one per account,
//! spawned alongside the re-decryption queue in
//! [`run_account`](crate::engine) — that pages each *joined* room's pre-existing
//! history backward through `/messages` and persists every event through the same
//! ingestion path as live sync
//! ([`persist_backfilled_event`](crate::engine::persist_backfilled_event)). So
//! hot columns, crypto siblings, redaction handling, the M8 aggregation indexes,
//! and the M9 search index all apply uniformly, and re-runs are idempotent
//! (`upsert_event` is `ON CONFLICT DO NOTHING`).
//!
//! Paging issues the `/messages` request directly rather than calling
//! [`Room::messages`](matrix_sdk::Room::messages), which additionally writes every
//! fetched event into the SDK's own event cache — a store axon never reads, since
//! Postgres holds the authoritative copy. Doing so duplicated all of history on
//! disk: on one live account, 390k orphaned rows / 1.8 GB of SQLite shadowing a
//! 728 MB `events` table. [`fetch_page`] replicates what `Room::messages` does
//! (send, then decrypt each event via the same [`matrix_sdk::Room::decrypt_event`]
//! the re-decryption queue uses) minus that cache write. See issue #287.
//!
//! It is *not* a one-shot boot sweep: the task lives for the whole supervised run
//! and re-polls `joined_rooms()` when idle, so a newly joined or re-joined room is
//! picked up without a restart. Per-room progress is persisted after every page
//! (`room_backfill`), so backfill resumes where it left off across restarts.
//!
//! A room whose pages keep failing is backed off and eventually parked rather than
//! retried every sweep (ADR 0121): each failure is classified ([`PageFailure`]),
//! and the count, class and next-attempt time are persisted on the room's cursor
//! so they survive restarts. Only a homeserver rejection of the saved token sends
//! a room back to its live timeline end; a slow page is retried from the same
//! token. A homeserver can keep working on a `/messages` request long after axon
//! has given up on it, so every retry adds load the client cannot see.
//!
//! A disk-space safety valve ([`BackfillHealth`]) pauses paging when free space on
//! the guarded filesystem (`sync.backfill_disk_guard_path`, default the sync data
//! dir) is low — backfill grows storage unbounded ("to room start"), so it must
//! not fill the disk. Live sync is unaffected; only backfill pauses.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axon_core::SyncConfig;
use axon_store::{RoomBackfillFailure, RoomBackfillState};
use matrix_sdk::config::RequestConfig;
use matrix_sdk::deserialized_responses::TimelineEvent;
use matrix_sdk::ruma::api::client::message::get_message_events;
use matrix_sdk::ruma::events::{
    AnySyncMessageLikeEvent, AnySyncTimelineEvent, AnyTimelineEvent, SyncMessageLikeEvent,
};
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::Client;
use tokio_util::sync::CancellationToken;

use crate::engine::{persist_backfilled_event, PersistContext};

/// The filesystem the disk-space valve guards: the configured override, else the
/// sync data dir. Shared by the valve (`disk_ok`) and the live `GET /v1/status`
/// free-space read ([`BackfillHealth::free_bytes`]).
pub(crate) fn guard_path(config: &SyncConfig) -> PathBuf {
    config
        .backfill_disk_guard_path
        .clone()
        .unwrap_or_else(|| config.data_dir.clone())
}

/// Delay before retrying a room after its first consecutive failure. Each further
/// failure doubles it, up to [`RETRY_MAX`] (ADR 0121).
const RETRY_BASE: Duration = Duration::from_secs(60);

/// Ceiling on the backoff between attempts on a failing room.
const RETRY_MAX: Duration = Duration::from_secs(60 * 60);

/// Consecutive failures after which a room is parked.
const PARK_AFTER: i32 = 5;

/// How long a parked room waits between attempts.
const PARK_FOR: Duration = Duration::from_secs(24 * 60 * 60);

/// The result of attempting to page one room once.
enum PageOutcome {
    /// Fetched and persisted at least one event — real backward progress.
    Paged,
    /// Nothing to do this sweep: the room is complete or at its depth cap.
    Idle,
    /// The homeserver did not serve the page; recorded against the room.
    Failed(PageFailure),
    /// Nothing to record against the room: cancellation, or a local store error
    /// after a page that the homeserver did serve. Retried next sweep.
    Abandoned,
}

/// Why the homeserver did not serve a backfill page — which decides what the
/// engine does next (ADR 0121).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageFailure {
    /// A `400` for a request that carried a resume token: the homeserver rejected
    /// the token. Clear it and page from the live end next sweep.
    Rejected,
    /// A `403` or `404`: the homeserver will not serve this room's history to the
    /// account. Park the room.
    Inaccessible,
    /// Anything else — axon's page timeout, a transport error, a `5xx`, a `429`.
    /// Retry the same token after a growing delay, then park.
    Retryable,
}

impl PageFailure {
    /// The `room_backfill.last_failure` value.
    fn as_str(self) -> &'static str {
        match self {
            PageFailure::Rejected => "rejected",
            PageFailure::Inaccessible => "inaccessible",
            PageFailure::Retryable => "retryable",
        }
    }

    /// Classify a failed `/messages` call. `had_token` is whether the request
    /// carried a `from` token: a `400` without one says nothing about a token.
    fn of(err: &matrix_sdk::Error, had_token: bool) -> Self {
        Self::of_status(
            err.as_client_api_error().map(|e| e.status_code.as_u16()),
            had_token,
        )
    }

    /// The status-code rule behind [`of`](Self::of), split out so it can be
    /// tested without standing up an SDK error. `None` is a failure with no
    /// homeserver response (transport error, timeout).
    fn of_status(status: Option<u16>, had_token: bool) -> Self {
        match status {
            Some(400) if had_token => PageFailure::Rejected,
            Some(403 | 404) => PageFailure::Inaccessible,
            _ => PageFailure::Retryable,
        }
    }
}

/// The backoff before the `failures`th consecutive failure's retry: `RETRY_BASE`
/// doubled `failures - 1` times, capped at `RETRY_MAX`.
fn retry_delay(failures: i32) -> Duration {
    let doublings = failures.saturating_sub(1).clamp(0, 16) as u32;
    RETRY_BASE.saturating_mul(1 << doublings).min(RETRY_MAX)
}

/// What a failure does to a room that had already failed `prior` times in a row,
/// and had its token rejected `prior_rejections` times in all — its delay,
/// whether it is now parked, and whether its token is dropped.
///
/// A first rejection restarts the room from its live end at once. A later one
/// parks it as well: the pages back down to the same depth succeed and reset
/// `prior`, so a homeserver that rejects the token at that depth every time would
/// otherwise be walked back to it again and again with no delay.
fn failure_record(
    failure: PageFailure,
    prior: i32,
    prior_rejections: i32,
) -> RoomBackfillFailure<'static> {
    let failures = prior.saturating_add(1);
    let (retry_after, parked, clear_token) = match failure {
        PageFailure::Rejected if prior_rejections > 0 => (PARK_FOR, true, true),
        PageFailure::Rejected => (Duration::ZERO, false, true),
        PageFailure::Inaccessible => (PARK_FOR, true, false),
        PageFailure::Retryable if failures >= PARK_AFTER => (PARK_FOR, true, false),
        PageFailure::Retryable => (retry_delay(failures), false, false),
    };
    RoomBackfillFailure {
        kind: failure.as_str(),
        retry_after,
        parked,
        clear_token,
    }
}

/// Persist a failed page against the room and log it. The transition into the
/// parked state is logged once at WARN; a parked room failing its daily retry
/// logs at INFO.
async fn record_failure(
    ctx: &PersistContext,
    room_id: &str,
    state: Option<&RoomBackfillState>,
    failure: PageFailure,
) {
    let account_id = ctx.account_id;
    let prior = state.map_or(0, |s| s.consecutive_failures);
    let prior_rejections = state.map_or(0, |s| s.token_rejections);
    let was_parked = state.is_some_and(|s| s.parked);
    let record = failure_record(failure, prior, prior_rejections);
    let failures = prior.saturating_add(1);
    let retry_in_secs = record.retry_after.as_secs();
    if record.parked && !was_parked {
        tracing::warn!(
            account_id = %account_id,
            room_id = %room_id,
            failure = record.kind,
            failures,
            retry_in_secs,
            "backfill: parking room after repeated failures"
        );
    } else if record.parked {
        tracing::info!(
            account_id = %account_id,
            room_id = %room_id,
            failure = record.kind,
            failures,
            retry_in_secs,
            "backfill: parked room failed again"
        );
    } else if record.clear_token {
        tracing::warn!(
            account_id = %account_id,
            room_id = %room_id,
            "backfill: homeserver rejected the resume token; restarting from the live end"
        );
    } else {
        tracing::info!(
            account_id = %account_id,
            room_id = %room_id,
            failure = record.kind,
            failures,
            retry_in_secs,
            "backfill: backing off room"
        );
    }
    if let Err(err) = ctx
        .store
        .record_room_backfill_failure(account_id, room_id, record)
        .await
    {
        tracing::warn!(account_id = %account_id, room_id = %room_id, error = %err, "backfill: record failure failed");
    }
}

/// Shared, cheaply-cloneable view of the backfill disk-space valve: whether it has
/// paused paging, and (computed live) how much free space is on the guarded
/// filesystem. Held by the engine and read by the API status surface; the paused
/// flag is written by the backfill task.
#[derive(Clone, Default)]
pub struct BackfillHealth {
    inner: Arc<BackfillHealthInner>,
}

#[derive(Default)]
struct BackfillHealthInner {
    paused_low_disk: AtomicBool,
    /// The filesystem whose free space is reported at `GET /v1/status`. `None`
    /// (test/default handles) reports 0 free.
    guard_path: Option<PathBuf>,
}

impl BackfillHealth {
    /// A handle reporting free space on `guard_path` (the valve's guarded
    /// filesystem). `None` — used by tests and the no-backfill default — reports 0.
    pub fn new(guard_path: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(BackfillHealthInner {
                paused_low_disk: AtomicBool::new(false),
                guard_path,
            }),
        }
    }

    /// Whether backfill is currently paused because free disk space is low.
    pub fn paused_low_disk(&self) -> bool {
        self.inner.paused_low_disk.load(Ordering::Relaxed)
    }

    /// Free bytes on the guarded filesystem, read **live** so `GET /v1/status` is
    /// accurate even when no backfill sweep has run recently (idle, disabled, or an
    /// account with no rooms). `0` if the path is unset or can't be stat'd.
    pub fn free_bytes(&self) -> u64 {
        self.inner
            .guard_path
            .as_ref()
            .and_then(|p| fs4::statvfs(p).ok())
            .map(|s| s.available_space())
            .unwrap_or(0)
    }

    /// Set the paused flag, returning the previous value so the caller can detect
    /// (and log) a transition.
    fn set_paused(&self, paused: bool) -> bool {
        self.inner.paused_low_disk.swap(paused, Ordering::Relaxed)
    }
}

/// Runtime knobs for the backfill task, derived from [`SyncConfig`].
#[derive(Clone)]
pub(crate) struct BackfillParams {
    page_size: u32,
    /// Per-room event cap (0 = unbounded / "to room start").
    target_depth: u64,
    throttle: Duration,
    /// Per-request timeout for one `/messages` page.
    page_timeout: Duration,
    idle_poll: Duration,
    min_free_bytes: u64,
    min_free_percent: f64,
    /// Filesystem whose free space gates backfill (the sync data dir).
    guard_path: PathBuf,
}

impl BackfillParams {
    pub(crate) fn from_config(config: &SyncConfig) -> Self {
        Self {
            page_size: config.backfill_page_size.max(1),
            target_depth: config.backfill_target_depth,
            throttle: Duration::from_millis(config.backfill_throttle_ms),
            page_timeout: Duration::from_secs(config.backfill_page_timeout_secs.max(1)),
            idle_poll: Duration::from_secs(config.backfill_idle_poll_secs.max(1)),
            min_free_bytes: config.backfill_min_free_bytes,
            min_free_percent: config.backfill_min_free_percent,
            guard_path: guard_path(config),
        }
    }
}

/// Whether there is enough free disk to keep backfilling, recording the result
/// (and logging pause/resume transitions) on `health`. A stat failure is treated
/// as "ok" (don't wedge backfill on an unreadable path) but logged.
fn disk_ok(params: &BackfillParams, health: &BackfillHealth, account_id: uuid::Uuid) -> bool {
    let (paused, free) = match fs4::statvfs(&params.guard_path) {
        Ok(stats) => {
            let free = stats.available_space();
            let total = stats.total_space();
            let pct = if total > 0 {
                (free as f64 / total as f64) * 100.0
            } else {
                100.0
            };
            let low = free < params.min_free_bytes || pct < params.min_free_percent;
            (low, free)
        }
        Err(err) => {
            tracing::warn!(
                account_id = %account_id,
                path = %params.guard_path.display(),
                error = %err,
                "backfill disk-space check failed; proceeding"
            );
            // Don't wedge backfill on an unreadable path. `free` here is only for
            // the transition log below; the live `GET /v1/status` free-space read
            // is separate.
            (false, 0)
        }
    };
    let was_paused = health.set_paused(paused);
    if was_paused != paused {
        if paused {
            tracing::warn!(account_id = %account_id, free_bytes = free, "backfill paused: low disk space");
        } else {
            tracing::info!(account_id = %account_id, free_bytes = free, "backfill resuming: disk space recovered");
        }
    }
    !paused
}

/// Run the backfill engine for one account until `cancel` fires.
pub(crate) async fn run(
    client: Client,
    ctx: PersistContext,
    params: BackfillParams,
    health: BackfillHealth,
    cancel: CancellationToken,
) {
    let account_id = ctx.account_id;
    tracing::debug!(account_id = %account_id, "history backfill task started");

    loop {
        if cancel.is_cancelled() {
            return;
        }

        // Load every room's cursor once per sweep (one query) rather than a lookup
        // per room, so a large account doesn't hammer the DB each pass.
        let cursors = match ctx.store.list_room_backfill(account_id).await {
            Ok(c) => c,
            Err(err) => {
                // Skip the sweep rather than run it with no cursors: every room
                // would look new, page from its live end, and overwrite its saved
                // deeper token with a shallower one (ADR 0121).
                tracing::warn!(account_id = %account_id, error = %err, "backfill: list cursors failed; skipping this sweep");
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(params.idle_poll) => {}
                }
                continue;
            }
        };

        let mut progressed = false;
        for room in client.joined_rooms() {
            if cancel.is_cancelled() {
                return;
            }
            let room_id = room.room_id().as_str().to_owned();

            // Skip already-complete rooms without any per-room work — the common
            // steady state on an established account, and what keeps a mostly-done
            // account from re-touching every room each sweep.
            let state = cursors.get(&room_id);
            if state.is_some_and(|s| s.complete) {
                continue;
            }
            // A failing room waits out its backoff or park interval (ADR 0121)
            // without a request — the point is not to add load upstream.
            if state.is_some_and(|s| !s.retry_due) {
                continue;
            }
            // Disk guard checked per active room (covers the first too, so there is
            // no redundant pre-loop check): if low, abandon the sweep and fall
            // through to the idle wait below, which re-checks next round.
            if !disk_ok(&params, &health, account_id) {
                break;
            }

            match backfill_room_page(&ctx, &params, &room, state.cloned(), &cancel).await {
                PageOutcome::Paged => {
                    progressed = true;
                    // Throttle *only* after real work, so completed/capped rooms
                    // don't add dead time that starves an actively-backfilling room
                    // on an account with many finished rooms.
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(params.throttle) => {}
                    }
                }
                PageOutcome::Idle | PageOutcome::Abandoned => {}
                PageOutcome::Failed(failure) => {
                    record_failure(&ctx, &room_id, state, failure).await;
                }
            }
        }

        // Nothing advanced this pass (every room complete/capped, paused, or
        // failing): idle, then re-poll — this is how newly joined/re-joined rooms
        // are picked up without a restart, and it caps the cursor-read cadence for
        // a fully-backfilled account at one sweep per `idle_poll`.
        if !progressed {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(params.idle_poll) => {}
            }
        }
    }
}

/// Decrypt one paged event into the [`TimelineEvent`] the ingestion path expects.
///
/// Mirrors the SDK's own `Room::try_decrypt_event`, which `Room::messages` applies
/// to every fetched event, with two deliberate differences: no push context (axon
/// never reads `push_actions`, so computing them is pure waste — the re-decryption
/// queue passes `None` for the same reason), and no event-cache write.
///
/// An event whose keys haven't arrived yet comes back as a UTD `TimelineEvent`
/// rather than an error, exactly as on the live path; the re-decryption queue
/// back-fills it once keys arrive.
async fn decrypt_paged_event(room: &matrix_sdk::Room, raw: Raw<AnyTimelineEvent>) -> TimelineEvent {
    if needs_decryption(&raw) {
        if let Ok(event) = room.decrypt_event(raw.cast_ref_unchecked(), None).await {
            return event;
        }
    }
    TimelineEvent::from_plaintext(raw.cast())
}

/// Whether a paged event is an original `m.room.encrypted` message-like event, and
/// so the one shape [`matrix_sdk::Room::decrypt_event`] accepts.
///
/// Split out from [`decrypt_paged_event`] because it is the classification the
/// bypass has to get right — everything else in the decrypt path is delegated to
/// the SDK. Anything else (plaintext, state, redacted) is passed through
/// undecrypted, matching the SDK's own `try_decrypt_event`.
fn needs_decryption(raw: &Raw<AnyTimelineEvent>) -> bool {
    matches!(
        raw.deserialize_as::<AnySyncTimelineEvent>(),
        Ok(AnySyncTimelineEvent::MessageLike(
            AnySyncMessageLikeEvent::RoomEncrypted(SyncMessageLikeEvent::Original(_))
        ))
    )
}

/// Why [`fetch_page`] returned no page.
enum FetchError {
    /// The homeserver answered with an error, or the request failed in transit.
    Homeserver(matrix_sdk::Error),
    /// The request was still unanswered after the page timeout.
    TimedOut,
}

/// Fetch and decrypt one `/messages` page, returning the events and the resume
/// token for the next page (`None` once the room's history is exhausted).
///
/// This is `Room::messages` minus its event-cache write — see the module docs and
/// issue #287. Everything else matches except retries: see [`send_page`].
///
/// `timeout` bounds the request only. Decryption is local work, so a slow decrypt
/// must not be recorded against the homeserver (ADR 0121); the caller's
/// cancellation still covers it.
async fn fetch_page(
    room: &matrix_sdk::Room,
    request: get_message_events::v3::Request,
    timeout: Duration,
) -> Result<(Vec<TimelineEvent>, Option<String>), FetchError> {
    // The SDK's own timeout is the same length and also ends in a retryable
    // failure, so it does not matter which fires first. This outer one also covers
    // time the SDK spends before the request is on the wire.
    let response =
        match tokio::time::timeout(timeout, send_page(&room.client(), request, timeout)).await {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => return Err(FetchError::Homeserver(err)),
            Err(_elapsed) => return Err(FetchError::TimedOut),
        };

    let mut events = Vec::with_capacity(response.chunk.len());
    for raw in response.chunk {
        events.push(decrypt_paged_event(room, raw).await);
    }

    // `state` is ignored: the paged timeline events are what backfill persists, and
    // room state arrives through live sync.
    Ok((events, response.end))
}

/// Send one `/messages` request with the SDK's internal retry disabled. The SDK
/// would otherwise retry a `5xx` or `429` with backoff inside the page timeout,
/// and each retry can start more federation work on the homeserver. The engine's
/// own backoff (ADR 0121) is the only retry. The SDK timeout is the configured
/// page timeout, so a raised `backfill_page_timeout_secs` is not cut short by the
/// SDK's 30 s default.
async fn send_page(
    client: &Client,
    request: get_message_events::v3::Request,
    timeout: Duration,
) -> Result<get_message_events::v3::Response, matrix_sdk::Error> {
    Ok(client
        .send(request)
        .with_request_config(RequestConfig::new().disable_retry().timeout(timeout))
        .await?)
}

/// Page one room one step older and persist the results, resuming from the saved
/// token (or the live timeline end when there is none — a first page, or after
/// the homeserver rejected the token).
async fn backfill_room_page(
    ctx: &PersistContext,
    params: &BackfillParams,
    room: &matrix_sdk::Room,
    state: Option<RoomBackfillState>,
    cancel: &CancellationToken,
) -> PageOutcome {
    let account_id = ctx.account_id;
    let room_id = room.room_id().as_str().to_owned();

    let (from_token, complete, count, failing) = match state {
        Some(s) => (
            s.oldest_seen_token,
            s.complete,
            s.events_backfilled,
            s.parked || s.consecutive_failures > 0,
        ),
        None => (None, false, 0, false),
    };
    if complete {
        return PageOutcome::Idle;
    }
    if params.target_depth != 0 && count as u64 >= params.target_depth {
        // A room can reach its cap with failures still recorded, when the cap is
        // lowered below a failing room's depth. It is not paged while the cap
        // stands, so no success would clear them, and status would count it as
        // parked.
        if failing {
            if let Err(err) = ctx
                .store
                .clear_room_backfill_failures(account_id, &room_id)
                .await
            {
                tracing::warn!(account_id = %account_id, room_id = %room_id, error = %err, "backfill: clear failures failed");
            }
        }
        return PageOutcome::Idle;
    }

    let mut request = get_message_events::v3::Request::backward(room.room_id().to_owned());
    request.limit = params.page_size.into();
    let had_token = from_token.is_some();
    request.from = from_token;

    // The `/messages` call crosses the homeserver boundary, so bound it: a timeout
    // on the request (inside `fetch_page`), and the cancellation token on the
    // request and the decryption after it. Without this, a hung request
    // would keep the backfill task parked inside the await, and shutdown / logout /
    // delete — which cancel the token and then await this task's handle — would
    // stall until the request eventually resolved.
    let (events, end) = tokio::select! {
        biased;
        _ = cancel.cancelled() => return PageOutcome::Abandoned,
        result = fetch_page(room, request, params.page_timeout) => match result {
            Ok(m) => m,
            Err(FetchError::Homeserver(err)) => {
                let failure = PageFailure::of(&err, had_token);
                tracing::warn!(account_id = %account_id, room_id = %room_id, error = %err, failure = failure.as_str(), "backfill: /messages page failed");
                return PageOutcome::Failed(failure);
            }
            Err(FetchError::TimedOut) => {
                // The homeserver may still be working on it: a timeout is never a
                // token rejection, so the same token is retried after a backoff.
                tracing::warn!(account_id = %account_id, room_id = %room_id, timeout_secs = params.page_timeout.as_secs(), "backfill: /messages page timed out");
                return PageOutcome::Failed(PageFailure::Retryable);
            }
        },
    };

    for tev in &events {
        if cancel.is_cancelled() {
            return PageOutcome::Abandoned;
        }
        persist_backfilled_event(ctx, room, tev).await;
    }

    let fetched = events.len();
    let complete = end.is_none();
    if let Err(err) = ctx
        .store
        .save_room_backfill(
            account_id,
            &room_id,
            end.as_deref(),
            complete,
            fetched as i64,
        )
        .await
    {
        // The events landed but the cursor didn't advance; don't count this as
        // progress (the next sweep re-pages the same window — idempotent for
        // `events`). The homeserver served the page, so nothing is recorded
        // against the room.
        tracing::warn!(account_id = %account_id, room_id = %room_id, error = %err, "backfill: save cursor failed");
        return PageOutcome::Abandoned;
    }

    tracing::debug!(
        account_id = %account_id,
        room_id = %room_id,
        fetched,
        complete,
        "backfilled a page"
    );

    if fetched > 0 {
        PageOutcome::Paged
    } else {
        PageOutcome::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DECRYPTED_MESSAGE: &str =
        include_str!("../../../tests/fixtures/decryption/decrypted_message_event.json");
    const UTD_ENCRYPTED: &str =
        include_str!("../../../tests/fixtures/decryption/utd_encrypted_event.json");

    fn raw(json: &str) -> Raw<AnyTimelineEvent> {
        serde_json::from_str(json).expect("fixture")
    }

    #[test]
    fn encrypted_event_is_routed_to_decryption() {
        assert!(needs_decryption(&raw(UTD_ENCRYPTED)));
    }

    #[test]
    fn plaintext_event_is_passed_through() {
        // The common case on an unencrypted room: no olm round-trip, straight to
        // `TimelineEvent::from_plaintext`.
        assert!(!needs_decryption(&raw(DECRYPTED_MESSAGE)));
    }

    #[test]
    fn redacted_encrypted_event_is_passed_through() {
        // A redacted `m.room.encrypted` has no ciphertext left to decrypt, so it
        // must take the plaintext arm rather than a doomed `decrypt_event` call.
        let redacted = json!({
            "type": "m.room.encrypted",
            "sender": "@alice:example.org",
            "event_id": "$redacted1:example.org",
            "origin_server_ts": 1_700_000_000_000_u64,
            "content": {},
            "unsigned": {
                "redacted_because": {
                    "type": "m.room.redaction",
                    "sender": "@alice:example.org",
                    "event_id": "$redaction1:example.org",
                    "origin_server_ts": 1_700_000_000_001_u64,
                    "content": {}
                }
            }
        });
        let raw: Raw<AnyTimelineEvent> = serde_json::from_value(redacted).expect("raw");
        assert!(!needs_decryption(&raw));
    }

    #[test]
    fn state_event_is_passed_through() {
        // Backfill pages state events too; only message-like encrypted events are
        // decryptable, so a state event must never reach `decrypt_event`.
        let raw: Raw<AnyTimelineEvent> = serde_json::from_value(json!({
            "type": "m.room.topic",
            "sender": "@alice:example.org",
            "event_id": "$topic1:example.org",
            "origin_server_ts": 1_700_000_000_000_u64,
            "state_key": "",
            "content": { "topic": "hello" }
        }))
        .expect("raw");
        assert!(!needs_decryption(&raw));
    }

    #[test]
    fn malformed_event_is_passed_through() {
        // Undeserializable JSON must not be mistaken for an encrypted event; it
        // falls through to `from_plaintext`, where the ingestion path logs and
        // skips it.
        let raw: Raw<AnyTimelineEvent> =
            serde_json::from_value(json!({ "nonsense": true })).expect("raw");
        assert!(!needs_decryption(&raw));
    }

    #[test]
    fn a_400_rejects_the_token_only_when_there_was_one() {
        assert_eq!(
            PageFailure::of_status(Some(400), true),
            PageFailure::Rejected
        );
        // No `from` means no token to reject: a 400 here is something else, and
        // dropping a (non-existent) token cannot fix it.
        assert_eq!(
            PageFailure::of_status(Some(400), false),
            PageFailure::Retryable
        );
    }

    #[test]
    fn a_refusal_is_inaccessible_with_or_without_a_token() {
        for had_token in [true, false] {
            assert_eq!(
                PageFailure::of_status(Some(403), had_token),
                PageFailure::Inaccessible
            );
            assert_eq!(
                PageFailure::of_status(Some(404), had_token),
                PageFailure::Inaccessible
            );
        }
    }

    #[test]
    fn slowness_and_server_errors_are_retryable() {
        // `None` is a failure with no response at all: a transport error, or the
        // SDK's own timeout.
        for status in [None, Some(401), Some(429), Some(500), Some(502), Some(504)] {
            assert_eq!(
                PageFailure::of_status(status, true),
                PageFailure::Retryable,
                "status {status:?}"
            );
        }
    }

    #[test]
    fn retry_delay_doubles_from_a_minute_and_caps_at_an_hour() {
        let mins = |n| retry_delay(n).as_secs() / 60;
        assert_eq!([mins(1), mins(2), mins(3), mins(4)], [1, 2, 4, 8]);
        assert_eq!(retry_delay(7), RETRY_MAX, "64 minutes is capped at 60");
        assert_eq!(retry_delay(i32::MAX), RETRY_MAX, "no overflow");
        assert_eq!(
            retry_delay(0),
            RETRY_BASE,
            "a non-positive count is the base"
        );
    }

    /// The #741 loop: a page that keeps timing out must never send the room back
    /// to its live end. It backs off on the same token, then parks.
    #[test]
    fn repeated_timeouts_back_off_then_park_without_dropping_the_token() {
        for prior in 0..PARK_AFTER - 1 {
            let record = failure_record(PageFailure::Retryable, prior, 0);
            assert!(
                !record.clear_token,
                "failure {} dropped the token",
                prior + 1
            );
            assert!(!record.parked, "failure {} parked early", prior + 1);
            assert_eq!(record.retry_after, retry_delay(prior + 1));
        }
        let fifth = failure_record(PageFailure::Retryable, PARK_AFTER - 1, 0);
        assert!(fifth.parked, "the fifth consecutive failure parks the room");
        assert!(!fifth.clear_token);
        assert_eq!(fifth.retry_after, PARK_FOR);
        // A parked room failing its daily retry stays parked.
        let again = failure_record(PageFailure::Retryable, PARK_AFTER + 3, 0);
        assert!(again.parked && !again.clear_token);
        assert_eq!(again.retry_after, PARK_FOR);
    }

    #[test]
    fn a_rejected_token_restarts_from_the_live_end_at_once() {
        let record = failure_record(PageFailure::Rejected, 0, 0);
        assert!(record.clear_token);
        assert!(!record.parked);
        assert_eq!(record.retry_after, Duration::ZERO);
        assert_eq!(record.kind, "rejected");
    }

    /// The pages back down to a rejected depth succeed, so `prior` is 0 again by
    /// the time the token there is rejected a second time. Only the rejection
    /// count stops the walk-back from repeating every sweep.
    #[test]
    fn a_second_rejection_parks_the_room_and_still_drops_the_token() {
        let record = failure_record(PageFailure::Rejected, 0, 1);
        assert!(record.clear_token, "the rejected token is still worthless");
        assert!(record.parked);
        assert_eq!(record.retry_after, PARK_FOR);
        assert_eq!(record.kind, "rejected");
    }

    #[test]
    fn an_inaccessible_room_parks_on_the_first_refusal() {
        let record = failure_record(PageFailure::Inaccessible, 0, 0);
        assert!(record.parked);
        assert!(!record.clear_token);
        assert_eq!(record.retry_after, PARK_FOR);
    }

    /// Classification against real SDK errors from a stand-in homeserver: the
    /// status rule above is only as good as what `as_client_api_error` reports
    /// for the bodies a homeserver (or the proxy in front of it) actually sends.
    mod against_a_homeserver {
        use std::net::SocketAddr;
        use std::sync::atomic::AtomicUsize;

        use axum::http::StatusCode;
        use axum::routing::get;
        use axum::{Json, Router};
        use matrix_sdk::ruma::RoomId;

        use super::*;

        const MESSAGES: &str = "/_matrix/client/v3/rooms/{room_id}/messages";

        async fn serve(router: Router) -> SocketAddr {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            addr
        }

        /// A client with a session, so `/messages` (which requires
        /// authentication) is actually sent rather than failing locally. Built
        /// with the SDK's default request config, so the tests exercise the
        /// retry setting `send_page` applies per request, not one set here.
        async fn client_for(addr: SocketAddr) -> Client {
            use matrix_sdk::authentication::matrix::MatrixSession;
            use matrix_sdk::store::RoomLoadSettings;
            use matrix_sdk::{SessionMeta, SessionTokens};

            let client = Client::builder()
                .homeserver_url(format!("http://{addr}"))
                .server_versions([matrix_sdk::ruma::api::MatrixVersion::V1_11])
                .build()
                .await
                .expect("client against the stand-in homeserver");
            client
                .matrix_auth()
                .restore_session(
                    MatrixSession {
                        meta: SessionMeta {
                            user_id: matrix_sdk::ruma::UserId::parse("@backfill:localhost")
                                .expect("test user id"),
                            device_id: "BACKFILLDEV".into(),
                        },
                        tokens: SessionTokens {
                            access_token: "backfill-token".to_owned(),
                            refresh_token: None,
                        },
                    },
                    RoomLoadSettings::default(),
                )
                .await
                .expect("restore a session so the request is sent");
            client
        }

        /// Page once against `router` and classify the failure.
        async fn classify(router: Router, from: Option<&str>, timeout: Duration) -> PageFailure {
            let client = client_for(serve(router).await).await;
            let room_id = RoomId::parse("!room:localhost").expect("room id");
            let mut request = get_message_events::v3::Request::backward(room_id);
            request.from = from.map(str::to_owned);
            let err = send_page(&client, request, timeout)
                .await
                .expect_err("the stand-in only fails");
            PageFailure::of(&err, from.is_some())
        }

        fn answering(status: StatusCode, body: serde_json::Value) -> Router {
            Router::new().route(
                MESSAGES,
                get(move || {
                    let body = body.clone();
                    async move { (status, Json(body)) }
                }),
            )
        }

        #[tokio::test]
        async fn synapse_rejecting_a_stream_token_is_rejected() {
            let router = answering(
                StatusCode::BAD_REQUEST,
                json!({ "errcode": "M_UNKNOWN", "error": "Invalid stream token" }),
            );
            let failure = classify(router, Some("t1-2_0_0"), Duration::from_secs(5)).await;
            assert_eq!(failure, PageFailure::Rejected);
        }

        #[tokio::test]
        async fn not_in_the_room_is_inaccessible() {
            let router = answering(
                StatusCode::FORBIDDEN,
                json!({ "errcode": "M_FORBIDDEN", "error": "User not in room" }),
            );
            let failure = classify(router, Some("t1-2_0_0"), Duration::from_secs(5)).await;
            assert_eq!(failure, PageFailure::Inaccessible);
        }

        /// A proxy's HTML 502 is retryable, and is sent exactly once: with the
        /// SDK's default config it would be retried with backoff inside the page
        /// timeout, each retry adding upstream work.
        #[tokio::test]
        async fn a_proxy_502_is_retryable_and_not_retried_by_the_sdk() {
            let hits = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&hits);
            let router = Router::new().route(
                MESSAGES,
                get(move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                    async { (StatusCode::BAD_GATEWAY, "<html>502 Bad Gateway</html>") }
                }),
            );
            // Bounded so a regression fails fast instead of hanging: the SDK's
            // default retries a 5xx for up to 15 minutes.
            let failure = tokio::time::timeout(
                Duration::from_secs(10),
                classify(router, Some("t1-2_0_0"), Duration::from_secs(5)),
            )
            .await
            .expect("the SDK kept retrying the 502");
            assert_eq!(failure, PageFailure::Retryable);
            assert_eq!(hits.load(Ordering::SeqCst), 1, "the SDK retried the page");
        }

        /// The #741 trigger: a homeserver that never answers in time. The SDK's
        /// own timeout fires (no response, so no status) and the page is
        /// retryable on the same token, not a rejection.
        #[tokio::test]
        async fn a_hung_page_is_retryable_even_with_a_token() {
            let router = Router::new().route(
                MESSAGES,
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    StatusCode::OK
                }),
            );
            let failure = classify(router, Some("t1-2_0_0"), Duration::from_millis(200)).await;
            assert_eq!(failure, PageFailure::Retryable);
        }
    }
}
