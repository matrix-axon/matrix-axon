//! Interactive SAS device verification (M7a PR6).
//!
//! Drives the matrix-rust-sdk SAS state machine for a logged-in account and
//! surfaces its progress as `verification.*` frames on the live-event bus. The
//! public surface is [`VerificationEngine`], the concrete backend the API's
//! verification port is adapted onto in `axon-server`.
//!
//! **Flows are ephemeral.** The in-flight flow — and the SDK's ephemeral SAS
//! keys — live only in the SDK's in-memory verification machine, which exposes no
//! way to serialize them, so a flow is re-readable across a *client* reconnect
//! (this process up) but never across a restart of this process. We mirror the
//! live SDK objects in an in-memory [`FlowRegistry`]; nothing is persisted. A
//! terminal flow's outcome is retained for [`TERMINAL_TTL`] so a reconnecting
//! client can still read whether it finished or was cancelled, then it is evicted
//! lazily. See ADR 0011 and the M7a PR6 plan for the full crash/reconnect matrix.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axon_core::{LiveFrame, VerificationFrame, VerificationFrameKind};
use axon_store::{Account, AccountState, Store};
use futures_util::{pin_mut, Stream, StreamExt};
use matrix_sdk::encryption::verification::{
    SasState, SasVerification, VerificationRequest, VerificationRequestState,
};
use matrix_sdk::event_handler::Ctx;
use matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent;
use matrix_sdk::ruma::events::key::verification::VerificationMethod;
use matrix_sdk::ruma::events::room::message::{MessageType, RoomMessageEventContent};
use matrix_sdk::ruma::events::OriginalSyncMessageLikeEvent;
use matrix_sdk::ruma::{OwnedDeviceId, OwnedRoomId, OwnedUserId, RoomId};
use matrix_sdk::{Client, Room};
use tokio::sync::{broadcast, mpsc, OwnedMutexGuard};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::error::GatewayError;
use crate::lifecycle::{lock_for, IdentityLocks};
use crate::manager::ClientManager;

/// The only verification method axon implements: short-auth-string (emoji /
/// decimal). QR is explicitly deferred (the PRD / ADR 0011), so every request we
/// start and every incoming request we accept advertises **SAS only** — otherwise
/// matrix-sdk's default method set (with the `qrcode` feature on) would let a peer
/// pick QR, which this driver would then immediately cancel.
fn sas_only() -> Vec<VerificationMethod> {
    vec![VerificationMethod::SasV1]
}

/// How long a terminal (`Done`/`Cancelled`) flow's outcome is kept readable after
/// it finishes, so a client that missed the one-shot terminal frame can still
/// read the result via `GET …/verify/{flow_id}` before the entry is evicted.
const TERMINAL_TTL: Duration = Duration::from_secs(300);

/// The stage of a flow, axon-sync-side. The `axon-server` adapter maps this to
/// the API's `FlowStage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowStage {
    /// A verification was requested; awaiting the peer.
    Requested,
    /// The peer is ready; SAS not yet computed.
    Ready,
    /// Keys exchanged — the SAS emoji/decimals are available to compare.
    KeysExchanged,
    /// This side has confirmed; awaiting the peer's MAC.
    Confirmed,
    /// Completed successfully; the device is now cross-signed.
    Done,
    /// Cancelled (timeout, mismatch, or either side cancelling).
    Cancelled,
}

/// A replayable snapshot of one flow, re-derived from the live SDK object (or a
/// recently-terminal flow's retained outcome). The `axon-server` adapter maps
/// this to the API's `FlowSummary`.
#[derive(Debug, Clone)]
pub struct FlowState {
    pub flow_id: String,
    /// The user being verified: the account's own user id for self-verification,
    /// or the peer's user id for cross-user verification (ADR 0040).
    pub target_user_id: String,
    pub target_device_id: Option<String>,
    pub stage: FlowStage,
    pub emoji: Option<Vec<(String, String)>>,
    pub decimals: Option<(u16, u16, u16)>,
    pub cancel_reason: Option<String>,
}

/// What can go wrong driving a verification flow, axon-sync-side. The
/// `axon-server` adapter collapses these onto the API's `VerifyError` (and thus
/// HTTP statuses).
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// No account exists for the given id.
    #[error("no such account: {0}")]
    NotFound(Uuid),
    /// No live or recently-terminal flow exists for the given id.
    #[error("no such flow: {0}")]
    FlowNotFound(String),
    /// The account is logged out (`deactivated`) — no live client to verify with.
    #[error("account is not active: {0}")]
    NotActive(Uuid),
    /// The account is mid-teardown (`deleting`).
    #[error("account is being deleted: {0}")]
    BeingDeleted(Uuid),
    /// The named device is not a known device of this account.
    #[error("unknown device: {0}")]
    UnknownDevice(String),
    /// The start request named no verification target.
    #[error("no verification target named")]
    NoTarget,
    /// The start request named both target forms.
    #[error("ambiguous verification target")]
    AmbiguousTarget,
    /// The named user can't be verified — invalid user id, or the SDK has no
    /// cross-signing identity for them (cross-user verification, ADR 0040).
    #[error("unknown user: {0}")]
    UnknownUser(String),
    /// The flow is not in a stage that permits the requested operation.
    #[error("flow not in a state for this operation: {0}")]
    WrongStage(String),
    /// The upstream homeserver rejected or failed a verification send, or the SDK
    /// failed in a way the caller can't fix.
    #[error("upstream error: {0}")]
    Upstream(String),
    /// A store failure.
    #[error("store error: {0}")]
    Store(String),
}

/// The terminal outcome retained for [`TERMINAL_TTL`] after a flow finishes.
#[derive(Debug, Clone)]
enum TerminalOutcome {
    Done,
    Cancelled(Option<String>),
}

/// One tracked flow: the live SDK request, its SAS object once it exists, and —
/// once terminal — the retained outcome plus when it became terminal (for TTL
/// eviction). The live objects are what `get`/`list` re-derive state from.
pub(crate) struct FlowEntry {
    request: VerificationRequest,
    sas: Option<SasVerification>,
    /// The user being verified — own user id (self-verification) or the peer's
    /// (cross-user, ADR 0040).
    target_user_id: String,
    target_device_id: Option<String>,
    /// The DM room a cross-user flow runs over (`None` for a to-device
    /// self-verification flow). The sliding-sync loop subscribes it while the flow
    /// is live so its events are delivered (ADR 0040).
    room_id: Option<OwnedRoomId>,
    terminal: Option<TerminalOutcome>,
    terminal_at: Option<Instant>,
    /// The token the flow's driver runs under. Held here so a sync-run teardown can
    /// cancel every in-flight flow of an account ([`cancel_account_flows`]) — a
    /// driver must never outlive the SDK client it was attached to (a supervised
    /// restart evicts and rebuilds that client).
    driver_cancel: CancellationToken,
    /// The driver task's handle, so [`cancel_account_flows`] can wait for it to
    /// actually exit (not just signal `driver_cancel`) before a restart rebuilds
    /// the client — otherwise the driver's `Client` clone (and the crypto store's
    /// whole connection pool behind it) outlives the run that owned it. `None`
    /// once taken for that wait, or if the driver has already removed this entry
    /// itself.
    driver_handle: Option<JoinHandle<()>>,
}

/// `(account_id, flow_id) → flow`. Shared (clone of the `Arc`) between the
/// [`VerificationEngine`] and every per-account incoming-request listener.
pub(crate) type FlowRegistry = Arc<Mutex<HashMap<(Uuid, String), FlowEntry>>>;

/// A fresh, empty flow registry. Owned by [`SyncEngine`](crate::SyncEngine).
pub(crate) fn new_registry() -> FlowRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

/// How long a freshly-joined candidate invite room stays explicitly subscribed
/// while we wait for a verification request to arrive in it. Element creates a DM
/// and sends the `m.key.verification.request` within seconds; if none arrives in
/// this window the room is dropped from the explicit subscription set (the Matrix
/// spec allows a request up to 10 minutes, but a real verification DM produces the
/// request immediately — a longer wait only keeps unrelated DMs subscribed).
const CANDIDATE_TTL: Duration = Duration::from_secs(90);

/// The Matrix verification request lifetime. Candidate invites can be dropped
/// from explicit subscriptions quickly, but a real request event that temporarily
/// outruns device-key availability must keep its room subscribed for the protocol
/// retry window.
const REQUEST_TTL: Duration = Duration::from_secs(600);
/// How long `cancel_account_flows` waits for a signalled driver to actually exit
/// before escalating to `abort()` — same escalation shape as lifecycle.rs's
/// `DRAIN_TIMEOUT`/`ABORT_TIMEOUT`. Shrunk under test so the escalation path (a
/// driver that ignores cancellation) runs in milliseconds.
#[cfg(not(test))]
const DRIVER_CANCEL_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const DRIVER_CANCEL_TIMEOUT: Duration = Duration::from_millis(250);

/// How long `join_or_abort_drivers` waits for a signalled driver to actually
/// exit before escalating to `abort()`. Deliberately larger than
/// `DRIVER_CANCEL_TIMEOUT` with real margin: a driver's own cancellation branch
/// runs a `cancel_verification_best_effort` bounded by `DRIVER_CANCEL_TIMEOUT`
/// *before* it calls `remove_flow`, and that inner timer starts at essentially
/// the same instant as this outer one (`driver_cancel.cancel()` fires
/// immediately before the handle is collected for joining) — without margin the
/// two race, and if the outer one wins, `abort()` drops the task mid-cleanup and
/// orphans the registry entry (caught in review of the #242 fix, see
/// `join_or_abort_drivers`'s own belt-and-suspenders cleanup below).
#[cfg(not(test))]
const DRIVER_JOIN_TIMEOUT: Duration = Duration::from_secs(8);
#[cfg(test)]
const DRIVER_JOIN_TIMEOUT: Duration = Duration::from_millis(600);

const DRIVER_SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a direct invite that failed the known-contact gate is remembered, so
/// `join_candidate_invites` doesn't re-scan it (an `O(joined_rooms)` membership
/// walk) on every 5s poll. Bounded so a membership change — we later share a room
/// with the inviter — is eventually reconsidered.
const REJECTED_INVITE_TTL: Duration = Duration::from_secs(300);

/// A set of keys each carrying an expiry deadline; expired keys are pruned lazily
/// on read, so membership reflects only un-expired entries with no background
/// sweep. This collapses the otherwise-identical `Instant`-deadline maps in this
/// module (candidate rooms, handled event ids, rejected invites) into one place.
///
/// (The terminal-flow grace sweep in [`sweep_expired`] is intentionally *not* built
/// on this: it retains domain entries of the flow registry by a `terminal_at`
/// *field*, keeping non-terminal entries forever — a different shape from a pure
/// key→deadline set.)
struct TtlSet<K: Eq + Hash> {
    entries: HashMap<K, Instant>,
}

// Manual `Default` (not derived): an empty set is valid for any key type, whereas
// `#[derive(Default)]` would wrongly demand `K: Default`.
impl<K: Eq + Hash> Default for TtlSet<K> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}

impl<K: Eq + Hash + Clone> TtlSet<K> {
    /// Insert `key` (or refresh its deadline) with `ttl` from now. Returns `true`
    /// if it was not already live — a fresh insert — and `false` if it merely
    /// refreshed a still-live entry. Callers that act only on a genuine change
    /// (e.g. waking the resubscribe loop) branch on this.
    fn insert(&mut self, key: K, ttl: Duration) -> bool {
        let now = Instant::now();
        self.prune(now);
        self.entries.insert(key, now + ttl).is_none()
    }

    /// Whether `key` is present and un-expired. Prunes expired entries first.
    fn contains(&mut self, key: &K) -> bool {
        let now = Instant::now();
        self.prune(now);
        self.entries.contains_key(key)
    }

    /// Forget `key` outright (regardless of its deadline).
    fn remove(&mut self, key: &K) {
        self.entries.remove(key);
    }

    /// The currently-live keys, pruning expired entries first.
    fn live(&mut self) -> Vec<K> {
        let now = Instant::now();
        self.prune(now);
        self.entries.keys().cloned().collect()
    }

    fn prune(&mut self, now: Instant) {
        self.entries.retain(|_, deadline| *deadline > now);
    }
}

/// Per-account set of rooms the sliding-sync loop should explicitly subscribe so
/// their timeline events are delivered regardless of the room's rank in the
/// selective window (ADR 0040). Cross-user verification is room-based, but a DM
/// outside the window receives no timeline events, so the verification request /
/// ready / accept / mac events never reach the handlers.
///
/// This holds only **candidate** rooms — DMs freshly joined from an invite that
/// may carry an incoming verification request — each with a TTL. The rooms of
/// *active* flows are derived separately from the flow registry
/// ([`active_flow_rooms`]); the loop subscribes the union of the two. The set is
/// therefore bounded by concurrent verifications plus recently-invited DMs, never
/// the whole DM list (the blast radius that sank the earlier attempt).
///
/// `RoomListService::subscribe_to_rooms` *replaces* all prior explicit
/// subscriptions, so the loop always re-subscribes the full union on any change;
/// the [`mpsc`] waker tells it when to.
#[derive(Clone, Default)]
pub(crate) struct VerificationRooms {
    inner: Arc<Mutex<HashMap<Uuid, AccountRooms>>>,
}

#[derive(Default)]
struct AccountRooms {
    /// Candidate invite rooms, each expiring from the subscription set after
    /// [`CANDIDATE_TTL`].
    candidates: TtlSet<OwnedRoomId>,
    /// Rooms that have already delivered an in-room verification request, but the
    /// SDK could not resolve it yet (usually because the sender's device keys have
    /// not arrived). Kept subscribed for the request lifetime, decoupled from the
    /// short candidate-invite window.
    pending_requests: TtlSet<OwnedRoomId>,
    /// Invites recently rejected by the direct/known-contact gate, so
    /// `join_candidate_invites` skips re-evaluating them every poll
    /// ([`REJECTED_INVITE_TTL`]).
    rejected: TtlSet<OwnedRoomId>,
    /// The current run's resubscribe waker, set by [`VerificationRooms::register`].
    waker: Option<RoomWaker>,
    /// A wake requested while no run waker was present. The next register gets an
    /// immediate recompute even if the request happened during restart backoff.
    pending_wake: bool,
    next_run_id: u64,
}

struct RoomWaker {
    run_id: u64,
    tx: mpsc::UnboundedSender<()>,
}

impl VerificationRooms {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Register the per-account sync loop's resubscribe waker, returning the
    /// receiver it awaits. Called once per run in `run_account`; an immediate wake
    /// is queued so any rooms added before registration are picked up.
    pub(crate) fn register(&self, account_id: Uuid) -> (u64, mpsc::UnboundedReceiver<()>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut guard = self.inner.lock().expect("verification rooms poisoned");
        let entry = guard.entry(account_id).or_default();
        entry.next_run_id = entry.next_run_id.wrapping_add(1).max(1);
        let run_id = entry.next_run_id;
        entry.waker = Some(RoomWaker {
            run_id,
            tx: tx.clone(),
        });
        // Always queue one recompute: it covers rooms added before this run
        // registered and active-flow rooms that live in the separate registry.
        entry.pending_wake = false;
        let _ = tx.send(());
        (run_id, rx)
    }

    /// Drop the account's resubscribe waker (its run is tearing down) but **keep**
    /// its candidate rooms. A candidate is a DM we've already *joined* from an
    /// invite and are holding subscribed while we wait for its
    /// `m.key.verification.request`; on a supervised restart inside that window the
    /// room is in `joined_rooms()`, not `invited_rooms()`, so `join_candidate_invites`
    /// can't re-add it — clearing it here would strand the verification with no
    /// recovery. The candidates self-expire via their TTL, and the new run's
    /// [`register`](Self::register) re-attaches a waker and replays them.
    pub(crate) fn unregister(&self, account_id: Uuid, run_id: u64) {
        if let Some(entry) = self
            .inner
            .lock()
            .expect("verification rooms poisoned")
            .get_mut(&account_id)
        {
            if entry.waker.as_ref().is_some_and(|w| w.run_id == run_id) {
                entry.waker = None;
            }
        }
    }

    fn wake_entry(entry: &mut AccountRooms) {
        if let Some(waker) = &entry.waker {
            let _ = waker.tx.send(());
        } else {
            entry.pending_wake = true;
        }
    }

    /// Add (or refresh the TTL of) a candidate invite room and wake the loop.
    pub(crate) fn add_candidate(&self, account_id: Uuid, room_id: OwnedRoomId) {
        let mut guard = self.inner.lock().expect("verification rooms poisoned");
        let entry = guard.entry(account_id).or_default();
        if entry.candidates.insert(room_id, CANDIDATE_TTL) {
            Self::wake_entry(entry);
        }
    }

    /// Whether `room_id` is a direct invite we recently rejected for failing the
    /// known-contact gate, so the poll can skip re-scanning it until the memo
    /// lapses (see [`REJECTED_INVITE_TTL`]).
    pub(crate) fn is_recently_rejected(&self, account_id: Uuid, room_id: &RoomId) -> bool {
        let mut guard = self.inner.lock().expect("verification rooms poisoned");
        guard
            .get_mut(&account_id)
            .is_some_and(|entry| entry.rejected.contains(&room_id.to_owned()))
    }

    /// Memoize a direct invite as rejected by the known-contact gate.
    pub(crate) fn mark_rejected(&self, account_id: Uuid, room_id: OwnedRoomId) {
        let mut guard = self.inner.lock().expect("verification rooms poisoned");
        guard
            .entry(account_id)
            .or_default()
            .rejected
            .insert(room_id, REJECTED_INVITE_TTL);
    }

    /// Keep a room subscribed after a real in-room request arrived but before the
    /// SDK can resolve its sender/device into a live request object.
    pub(crate) fn add_pending_request(&self, account_id: Uuid, room_id: OwnedRoomId) {
        let mut guard = self.inner.lock().expect("verification rooms poisoned");
        let entry = guard.entry(account_id).or_default();
        if entry.pending_requests.insert(room_id, REQUEST_TTL) {
            Self::wake_entry(entry);
        }
    }

    /// Wake the account's loop to recompute and re-subscribe (e.g. a flow we just
    /// started added an active-flow room the loop derives from the registry).
    pub(crate) fn wake(&self, account_id: Uuid) {
        if let Some(entry) = self
            .inner
            .lock()
            .expect("verification rooms poisoned")
            .get_mut(&account_id)
        {
            Self::wake_entry(entry);
        }
    }

    /// Drop expired rooms and return the account's current request-room hold set.
    /// Called by the loop just before it computes the subscription union.
    pub(crate) fn live_candidates(&self, account_id: Uuid) -> Vec<OwnedRoomId> {
        let mut guard = self.inner.lock().expect("verification rooms poisoned");
        let Some(entry) = guard.get_mut(&account_id) else {
            return Vec::new();
        };
        let mut rooms = entry.candidates.live();
        rooms.extend(entry.pending_requests.live());
        rooms
    }

    /// Remove `room_id` from the candidate set. Called once the room has produced a
    /// real verification request: it's now an active-flow room kept subscribed via
    /// the registry ([`active_flow_rooms`]), so it no longer needs the TTL'd
    /// candidate slot. Only touches this in-memory set — the "promotion" to an
    /// active-flow room is implicit in the registry, not written here. Idempotent.
    pub(crate) fn clear_candidate(&self, account_id: Uuid, room_id: &RoomId) {
        if let Some(entry) = self
            .inner
            .lock()
            .expect("verification rooms poisoned")
            .get_mut(&account_id)
        {
            entry.candidates.remove(&room_id.to_owned());
            entry.pending_requests.remove(&room_id.to_owned());
        }
    }
}

/// How long a handled room-event id is remembered for dedup. Bound to the Matrix
/// request lifetime, not the short candidate-invite window: a room-based
/// self-verification request can be re-delivered while the SAS flow is still
/// valid, and minting a fresh to-device request for that duplicate would produce
/// competing prompts.
const HANDLED_EVENT_TTL: Duration = REQUEST_TTL;

/// Bounded dedup memory for incoming room verification events (ADR 0040). A room
/// event can be re-delivered (reconnect, backfill), and the outgoing flow id of a
/// self-verification counter-request differs from the event id, so the registry's
/// flow-id dedup alone isn't enough to suppress a second delivery.
///
/// For the resolved cross-user path the id is recorded only *after* the event is
/// fully handled ([`mark`]); the early [`seen`] check is a non-committing peek.
/// That ordering is deliberate: a cross-user request's first delivery routinely
/// races ahead of the peer's device keys, so the handler bails and relies on
/// sliding-sync re-delivering the same event — committing the dedup mark on that
/// miss would suppress the only recovery path and strand the flow.
///
/// The room-based *self*-verification fallback can't use that ordering: it mints a
/// fresh to-device request (new flow id) per delivery, so two deliveries racing
/// before either records the id would mint two competing requests. That path
/// instead atomically claims the id up front with [`mark_if_new`] and rolls the
/// claim back with [`forget`] if minting fails, so a real failure still retries.
///
/// [`mark`]: HandledRoomEvents::mark
/// [`seen`]: HandledRoomEvents::seen
/// [`mark_if_new`]: HandledRoomEvents::mark_if_new
/// [`forget`]: HandledRoomEvents::forget
#[derive(Clone, Default)]
pub(crate) struct HandledRoomEvents {
    inner: Arc<Mutex<TtlSet<String>>>,
}

impl HandledRoomEvents {
    /// True if `event_id` was already handled within the TTL (a re-delivery the
    /// caller should drop). Prunes expired entries as a side effect; does **not**
    /// record the id — call [`mark`](Self::mark) once the event is fully handled.
    pub(crate) fn seen(&self, event_id: &str) -> bool {
        self.inner
            .lock()
            .expect("handled_room_events poisoned")
            .contains(&event_id.to_owned())
    }

    /// Record `event_id` as handled, so a later re-delivery is dropped by
    /// [`seen`](Self::seen) until the TTL lapses.
    pub(crate) fn mark(&self, event_id: String) {
        self.inner
            .lock()
            .expect("handled_room_events poisoned")
            .insert(event_id, HANDLED_EVENT_TTL);
    }

    /// Atomically claim `event_id`: record it and return `true` only if it was not
    /// already handled. A racing re-delivery gets `false` and bails, so the caller
    /// (the self-verification fallback) mints at most one to-device request per
    /// event. Pair a `false`-on-failure path with [`forget`](Self::forget).
    pub(crate) fn mark_if_new(&self, event_id: String) -> bool {
        self.inner
            .lock()
            .expect("handled_room_events poisoned")
            .insert(event_id, HANDLED_EVENT_TTL)
    }

    /// Release a previously [`mark_if_new`](Self::mark_if_new)-claimed id so a
    /// re-delivery is processed again — used when handling bailed after the claim
    /// (the mint failed) and we want the retry to land.
    pub(crate) fn forget(&self, event_id: &str) {
        self.inner
            .lock()
            .expect("handled_room_events poisoned")
            .remove(&event_id.to_owned());
    }
}

/// The rooms of every non-terminal flow of `account_id` that runs over a room
/// (cross-user, ADR 0040). The sliding-sync loop subscribes these so a flow's
/// ready/accept/mac events keep arriving; a terminal flow drops out on the next
/// recompute, releasing its subscription.
pub(crate) fn active_flow_rooms(registry: &FlowRegistry, account_id: Uuid) -> Vec<OwnedRoomId> {
    registry
        .lock()
        .expect("flow registry poisoned")
        .iter()
        .filter(|((aid, _), entry)| *aid == account_id && entry.terminal.is_none())
        .filter_map(|(_, entry)| entry.room_id.clone())
        .collect()
}

/// How often the background reaper sweeps expired terminal flows, so the grace
/// TTL holds even with no verify API traffic to trigger a lazy sweep.
const REAP_INTERVAL: Duration = Duration::from_secs(60);

/// Drop terminal entries whose grace window has elapsed. Called both lazily from
/// the read/start verbs and from the background [`reap_expired_flows`] task, so an
/// idle account's terminal flows still get reclaimed.
fn sweep_expired(registry: &FlowRegistry) {
    let now = Instant::now();
    registry
        .lock()
        .expect("flow registry poisoned")
        .retain(|_, e| match e.terminal_at {
            Some(at) => now.duration_since(at) < TERMINAL_TTL,
            None => true,
        });
}

/// Remove a flow outright (not retained for the grace TTL). Used when a driver
/// exits without a terminal outcome — cancelled mid-flight (e.g. the account
/// logged out) or the SDK stream closed — so the entry can't linger forever with
/// `terminal_at = None`, which the TTL sweep would never reclaim.
fn remove_flow(registry: &FlowRegistry, key: &(Uuid, String)) {
    registry.lock().expect("flow registry poisoned").remove(key);
}

async fn cancel_verification_best_effort<F, E>(
    account_id: Uuid,
    flow_id: &str,
    kind: &str,
    cancel: F,
) where
    F: Future<Output = Result<(), E>>,
{
    if tokio::time::timeout(DRIVER_CANCEL_TIMEOUT, cancel)
        .await
        .is_err()
    {
        tracing::warn!(
            account_id = %account_id,
            %flow_id,
            kind,
            timeout_secs = DRIVER_CANCEL_TIMEOUT.as_secs(),
            "timed out cancelling verification during teardown"
        );
    }
}

/// Cancel the driver of every non-terminal flow belonging to `account_id`, and
/// wait for each to actually exit before returning. Called from
/// `engine::run_account` when a sync run tears down or is rebuilt, so a
/// verification driver never keeps running against an evicted SDK client.
///
/// Signaling `driver_cancel` alone isn't enough: each driver holds a `Client`
/// clone, and until it drops, so does the crypto store's whole connection pool
/// behind it (GH #242 — this is a confirmed leak source, not a theoretical one).
/// A driver attached to the engine token would otherwise survive a supervised
/// restart, so this is what actually binds the flow's lifetime to the *run*.
/// Bounded by [`DRIVER_JOIN_TIMEOUT`] per driver, with a forced `abort()` on
/// timeout so a stuck driver still can't hold the pool open indefinitely. All
/// drivers for the account are joined concurrently, so this doesn't scale with
/// the (normally-rare) count of concurrent flows on one account.
pub(crate) async fn cancel_account_flows(registry: &FlowRegistry, account_id: Uuid) {
    let handles: Vec<(String, JoinHandle<()>)> = {
        let mut reg = registry.lock().expect("flow registry poisoned");
        reg.iter_mut()
            .filter(|((aid, _), entry)| *aid == account_id && entry.terminal.is_none())
            .filter_map(|((_, flow_id), entry)| {
                entry.driver_cancel.cancel();
                entry.driver_handle.take().map(|h| (flow_id.clone(), h))
            })
            .collect()
    };
    join_or_abort_drivers(registry, account_id, handles).await;
}

/// Wait for each already-signalled driver to exit (concurrently, not one at a
/// time), escalating to `abort()` on a per-driver [`DRIVER_JOIN_TIMEOUT`] so a
/// driver stuck in non-yielding code (or one that ignores its cancellation
/// token entirely) still can't hold its `Client` clone — and the crypto store
/// pool behind it — open indefinitely. An aborted driver that never reached a
/// terminal outcome has its registry entry cleaned up here too, since it can no
/// longer do that for itself.
async fn join_or_abort_drivers(
    registry: &FlowRegistry,
    account_id: Uuid,
    handles: Vec<(String, JoinHandle<()>)>,
) {
    let joins = handles.into_iter().map(|(flow_id, mut handle)| async move {
        if tokio::time::timeout(DRIVER_JOIN_TIMEOUT, &mut handle)
            .await
            .is_err()
        {
            tracing::warn!(
                account_id = %account_id,
                %flow_id,
                timeout_secs = DRIVER_JOIN_TIMEOUT.as_secs(),
                "verification driver did not exit after cancellation; aborting"
            );
            handle.abort();
            // The aborted task may have been killed mid-way through its own
            // `remove_flow` (or before reaching it), which would otherwise orphan
            // this entry forever — `sweep_expired` only reclaims entries with
            // `terminal_at` set. Only clean up if it's still non-terminal: a task
            // that reached `mark_terminal` before being aborted (e.g. wedged on a
            // full `live_tx` send after already marking Done/Cancelled) must keep
            // its outcome for the grace TTL, not have it erased here.
            let key = (account_id, flow_id);
            let still_pending = registry
                .lock()
                .expect("flow registry poisoned")
                .get(&key)
                .is_some_and(|entry| entry.terminal.is_none());
            if still_pending {
                remove_flow(registry, &key);
            }
        }
    });
    futures_util::future::join_all(joins).await;
}

/// Background task: periodically reclaim terminal flows past their grace TTL.
/// Spawned once per engine; runs until `cancel` fires. Without it the TTL would
/// hold only while verify API calls keep arriving to drive the lazy sweep.
pub(crate) async fn reap_expired_flows(registry: FlowRegistry, cancel: CancellationToken) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(REAP_INTERVAL) => sweep_expired(&registry),
        }
    }
}

/// Map a SAS emoji array into the wire-neutral `(symbol, description)` pairs.
fn emoji_pairs(sas: &SasVerification) -> Option<Vec<(String, String)>> {
    sas.emoji().map(|emojis| {
        emojis
            .iter()
            .map(|e| (e.symbol.to_owned(), e.description.to_owned()))
            .collect()
    })
}

fn request_state_label(state: &VerificationRequestState) -> &'static str {
    match state {
        VerificationRequestState::Created { .. } => "created",
        VerificationRequestState::Requested { .. } => "requested",
        VerificationRequestState::Ready { .. } => "ready",
        VerificationRequestState::Transitioned { .. } => "transitioned",
        VerificationRequestState::Done => "done",
        VerificationRequestState::Cancelled(_) => "cancelled",
    }
}

fn sas_state_label(state: &SasState) -> &'static str {
    match state {
        SasState::Created { .. } => "created",
        SasState::Started { .. } => "started",
        SasState::Accepted { .. } => "accepted",
        SasState::KeysExchanged { .. } => "keys_exchanged",
        SasState::Confirmed => "confirmed",
        SasState::Done { .. } => "done",
        SasState::Cancelled(_) => "cancelled",
    }
}

/// SAS-object stage for [`snapshot`]. `Done` here is the MAC verdict.
fn flow_stage_from_sas(sas: &SasVerification) -> FlowStage {
    match sas.state() {
        SasState::Created { .. } | SasState::Started { .. } | SasState::Accepted { .. } => {
            FlowStage::Ready
        }
        SasState::KeysExchanged { .. } => FlowStage::KeysExchanged,
        SasState::Confirmed => FlowStage::Confirmed,
        SasState::Done { .. } => FlowStage::Done,
        SasState::Cancelled(_) => FlowStage::Cancelled,
    }
}

/// Request-only stage when no SAS object is in hand.
///
/// A request-level [`VerificationRequestState::Done`] is the peer's
/// `m.key.verification.done`, not a MAC. Reporting [`FlowStage::Done`] from it
/// would let `GET /v1/verify/{flow}` say the device is verified with nothing
/// verified. Treat it as cancelled instead; the SAS path is the only way to
/// reach [`FlowStage::Done`] without a retained [`TerminalOutcome::Done`].
fn flow_stage_from_request_without_sas(state: &VerificationRequestState) -> FlowStage {
    match state {
        VerificationRequestState::Created { .. } | VerificationRequestState::Requested { .. } => {
            FlowStage::Requested
        }
        VerificationRequestState::Ready { .. } | VerificationRequestState::Transitioned { .. } => {
            FlowStage::Ready
        }
        VerificationRequestState::Done | VerificationRequestState::Cancelled(_) => {
            FlowStage::Cancelled
        }
    }
}

/// SAS currently attached to this request, if the request is still
/// `Transitioned`. Once the request itself is `Done` the SDK drops that object
/// from the request state; it then lives on [`FlowEntry::sas`] (if we already
/// adopted it) or in the SDK verification cache. Do not call this after
/// observing [`VerificationRequestState::Done`] — it cannot fire.
fn sas_from_request(request: &VerificationRequest) -> Option<SasVerification> {
    match request.state() {
        VerificationRequestState::Transitioned { verification } => verification.sas(),
        _ => None,
    }
}

fn registry_sas(registry: &FlowRegistry, key: &(Uuid, String)) -> Option<SasVerification> {
    registry
        .lock()
        .expect("flow registry poisoned")
        .get(key)
        .and_then(|entry| entry.sas.clone())
}

/// SAS to drive after the request reports `Done`.
///
/// `sas_from_request` is `None` here: the SDK has already dropped the object
/// from `request.state()`. Prefer a SAS we already stashed on the registry;
/// otherwise look in the SDK cache, which still holds the verification after
/// the request itself is `Done` (a coalesced `changes()` burst can skip
/// `Transitioned` and land on `Done`).
async fn sas_after_request_done(
    registry: &FlowRegistry,
    key: &(Uuid, String),
    client: &Client,
    other_user_id: &str,
    flow_id: &str,
) -> Option<SasVerification> {
    if let Some(sas) = registry_sas(registry, key) {
        return Some(sas);
    }
    let user_id = other_user_id.parse::<OwnedUserId>().ok()?;
    client
        .encryption()
        .get_verification(&user_id, flow_id)
        .await
        .and_then(|verification| verification.sas())
}

fn snapshot_from_sas(
    flow_id: String,
    target_user_id: String,
    target_device_id: Option<String>,
    sas: &SasVerification,
) -> FlowState {
    FlowState {
        flow_id,
        target_user_id,
        target_device_id,
        stage: flow_stage_from_sas(sas),
        emoji: emoji_pairs(sas),
        decimals: sas.decimals(),
        cancel_reason: sas.cancel_info().map(|c| c.reason().to_owned()),
    }
}

/// Re-derive a flow's replayable state from its live SDK object, or its retained
/// terminal outcome.
fn snapshot(entry: &FlowEntry) -> FlowState {
    let flow_id = entry.request.flow_id().to_owned();
    let target_user_id = entry.target_user_id.clone();
    let target_device_id = entry.target_device_id.clone();

    if let Some(outcome) = &entry.terminal {
        let (stage, cancel_reason) = match outcome {
            TerminalOutcome::Done => (FlowStage::Done, None),
            TerminalOutcome::Cancelled(reason) => (FlowStage::Cancelled, reason.clone()),
        };
        return FlowState {
            flow_id,
            target_user_id,
            target_device_id,
            stage,
            emoji: None,
            decimals: None,
            cancel_reason,
        };
    }

    if let Some(sas) = &entry.sas {
        snapshot_from_sas(flow_id, target_user_id, target_device_id, sas)
    } else if let Some(sas) = sas_from_request(&entry.request) {
        snapshot_from_sas(flow_id, target_user_id, target_device_id, &sas)
    } else {
        FlowState {
            flow_id,
            target_user_id,
            target_device_id,
            stage: flow_stage_from_request_without_sas(&entry.request.state()),
            emoji: None,
            decimals: None,
            cancel_reason: entry.request.cancel_info().map(|c| c.reason().to_owned()),
        }
    }
}

/// Publish a verification frame to the live-event bus, skipping the work when no
/// `/v1/ws` client is listening (mirrors the timeline publish path).
#[allow(clippy::too_many_arguments)]
fn publish(
    live_tx: &broadcast::Sender<LiveFrame>,
    account_id: Uuid,
    flow_id: &str,
    target_user_id: &str,
    target_device_id: Option<&str>,
    kind: VerificationFrameKind,
    emoji: Option<Vec<(String, String)>>,
    decimals: Option<(u16, u16, u16)>,
    outcome: Option<String>,
) {
    if live_tx.receiver_count() == 0 {
        return;
    }
    let _ = live_tx.send(LiveFrame::Verification(VerificationFrame {
        account_id,
        flow_id: flow_id.to_owned(),
        kind,
        target_user_id: target_user_id.to_owned(),
        target_device_id: target_device_id.map(ToOwned::to_owned),
        emoji,
        decimals,
        outcome,
    }));
}

/// A `WrongStage` error describing a flow that was cancelled — so `confirm` on a
/// cancelled flow is reported as a failure, never as a successful confirmation.
fn cancelled_err(reason: Option<&str>) -> VerifyError {
    match reason {
        Some(r) => VerifyError::WrongStage(format!("flow was cancelled: {r}")),
        None => VerifyError::WrongStage("flow was cancelled".to_owned()),
    }
}

/// Mark a flow terminal in the registry (so a reconnecting client reads the
/// outcome within the grace window) and stamp the eviction clock.
fn mark_terminal(registry: &FlowRegistry, key: &(Uuid, String), outcome: TerminalOutcome) -> bool {
    if let Some(entry) = registry
        .lock()
        .expect("flow registry poisoned")
        .get_mut(key)
    {
        entry.terminal = Some(outcome);
        entry.terminal_at = Some(Instant::now());
        entry.room_id.is_some()
    } else {
        false
    }
}

/// Shared context for the per-account incoming-request listener (added to each
/// account's client as an event-handler context in `engine::run_account`).
#[derive(Clone)]
pub(crate) struct VerificationListenerCtx {
    pub(crate) account_id: Uuid,
    pub(crate) registry: FlowRegistry,
    pub(crate) live_tx: broadcast::Sender<LiveFrame>,
    pub(crate) tracker: TaskTracker,
    /// The account's cancellation token: drivers spawned for incoming requests run
    /// under a child of it, so they end when the account stops.
    pub(crate) cancel: CancellationToken,
    /// Rooms the sliding-sync loop should keep subscribed for verification (ADR
    /// 0040). The room handler promotes the request's room out of the candidate
    /// set and wakes the loop so it stays subscribed as an active-flow room.
    pub(crate) rooms: VerificationRooms,
    /// Dedup memory for incoming room verification events (see
    /// [`HandledRoomEvents`]).
    pub(crate) handled_room_events: HandledRoomEvents,
}

/// Event handler for peer-initiated `m.key.verification.request` to-device events.
/// Registers the request and spawns a driver — there is no HTTP kickoff, so this
/// is the only place a peer-initiated flow surfaces (as a `verification.requested`
/// frame).
pub(crate) async fn on_incoming_request(
    ev: ToDeviceKeyVerificationRequestEvent,
    client: Client,
    ctx: Ctx<VerificationListenerCtx>,
) {
    let flow_id = ev.content.transaction_id.to_string();
    let Some(request) = client
        .encryption()
        .get_verification_request(&ev.sender, &flow_id)
        .await
    else {
        return;
    };

    // M7a only verifies axon's *own* device against another of the user's trusted
    // devices. Actively verifying another user's identity is explicitly out of
    // scope, so reject (cancel) anything that isn't a self-verification rather than
    // registering and accepting it. This also keeps the registry's `(account_id,
    // flow_id)` key unambiguous: only the user's own devices initiate, so the
    // transaction id can't collide with a different sender's.
    if !request.is_self_verification() {
        tracing::debug!(
            account_id = %ctx.account_id, %flow_id, sender = %ev.sender,
            "ignoring non-self verification request (out of scope)"
        );
        cancel_verification_best_effort(ctx.account_id, &flow_id, "request", request.cancel())
            .await;
        return;
    }

    // Self-verification only on the to-device path (cross-user is room-based, ADR
    // 0040), so the user being verified is our own user — the request sender.
    let target_user_id = ev.sender.to_string();
    let target_device_id = ev.content.from_device.to_string();
    let key = (ctx.account_id, flow_id);
    let driver_cancel = ctx.cancel.child_token();

    {
        let mut reg = ctx.registry.lock().expect("flow registry poisoned");
        // A duplicate request event (re-delivered on reconnect) must not spawn a
        // second driver for a flow we already track.
        if reg.contains_key(&key) {
            return;
        }
        reg.insert(
            key.clone(),
            FlowEntry {
                request: request.clone(),
                sas: None,
                target_user_id: target_user_id.clone(),
                target_device_id: Some(target_device_id.clone()),
                room_id: None,
                terminal: None,
                terminal_at: None,
                driver_cancel: driver_cancel.clone(),
                driver_handle: None,
            },
        );
        // Spawn and attach the handle while still holding the lock: otherwise
        // `cancel_account_flows` could run in the gap between insert and attach,
        // see `driver_handle: None` above, take a `None` handle, and return
        // without waiting for (or bounding) this driver's exit — reproducing a
        // narrow version of the #242 leak this registration exists to prevent.
        let handle = ctx.tracker.spawn(drive_request(
            request,
            FlowDriverCtx {
                account_id: ctx.account_id,
                target_user_id: target_user_id.clone(),
                target_device_id: Some(target_device_id.clone()),
                registry: ctx.registry.clone(),
                live_tx: ctx.live_tx.clone(),
                rooms: ctx.rooms.clone(),
                cancel: driver_cancel,
                client: client.clone(),
            },
        ));
        reg.get_mut(&key)
            .expect("just inserted above")
            .driver_handle = Some(handle);
    }

    tracing::info!(
        account_id = %ctx.account_id,
        flow_id = %key.1,
        target_user_id = %target_user_id,
        target_device_id = %target_device_id,
        "registered incoming to-device verification request"
    );
}

/// Event handler for peer-initiated in-room `m.key.verification.request` messages.
/// Element and other modern clients send a verification request as an
/// `m.room.message` (msgtype `m.key.verification.request`) in a DM rather than as a
/// to-device event. This is the transport for **cross-user** verification (ADR
/// 0040); it also handles room-based **self**-verification for clients that use it.
///
/// Two cases need different handling:
///
/// * **Cross-user** (sender ≠ our user): the SDK stores the request normally and
///   `get_verification_request()` returns it; we accept it through [`drive_request`].
/// * **Self-verification by room** (sender == our user, other device): the SDK's
///   `event_sent_from_us` guard treats any room event from our own user as
///   sent-by-us and drops it, so `get_verification_request()` returns `None`. We
///   fall back to initiating a to-device request to the sending device, which that
///   device shows as an incoming prompt.
pub(crate) async fn on_incoming_room_request(
    ev: OriginalSyncMessageLikeEvent<RoomMessageEventContent>,
    room: Room,
    client: Client,
    ctx: Ctx<VerificationListenerCtx>,
) {
    let MessageType::VerificationRequest(content) = &ev.content.msgtype else {
        return;
    };
    let event_id = ev.event_id.to_string();
    let room_id = room.room_id().to_owned();
    let from_device = content.from_device.to_string();

    // Ignore our own request echoed back into the room timeline.
    if Some(content.from_device.as_ref()) == client.device_id() {
        return;
    }

    // Dedup re-delivered events (reconnect / backfill): the outgoing flow id of the
    // self-verification counter-request differs from this event id, so the
    // registry's flow-id dedup alone wouldn't catch a second delivery. This is a
    // non-committing peek — the id is recorded only after the event is fully
    // handled (below), so a transient first-delivery miss stays eligible for the
    // re-delivery that recovers it.
    if ctx.handled_room_events.seen(&event_id) {
        return;
    }

    let request = if let Some(req) = client
        .encryption()
        .get_verification_request(&ev.sender, &ev.event_id)
        .await
    {
        req
    } else if Some(ev.sender.as_ref()) == client.user_id() {
        // Self-verification by room: the SDK dropped the request, so we mint a fresh
        // to-device request back to the sending device. Each mint gets a *new* flow
        // id, so the registry's flow-id dedup can't suppress a re-delivery — two
        // deliveries racing here would mint two competing requests (two SAS prompts
        // on the peer). Claim the event id atomically up front instead, so at most
        // one request is minted; a racing re-delivery loses the claim and bails.
        if !ctx.handled_room_events.mark_if_new(event_id.clone()) {
            return;
        }
        let device = match client
            .encryption()
            .get_device(&ev.sender, &content.from_device)
            .await
        {
            Ok(Some(device)) => device,
            // No mint happened, so release the claim — a re-delivery (e.g. once the
            // device's keys have loaded) should be allowed to retry.
            Ok(None) => {
                ctx.handled_room_events.forget(&event_id);
                return;
            }
            Err(err) => {
                ctx.handled_room_events.forget(&event_id);
                tracing::warn!(
                    account_id = %ctx.account_id, %room_id, %event_id, %from_device, error = %err,
                    "failed to fetch device for self-verification room request"
                );
                return;
            }
        };
        match device.request_verification_with_methods(sas_only()).await {
            Ok(req) => req,
            Err(err) => {
                ctx.handled_room_events.forget(&event_id);
                tracing::warn!(
                    account_id = %ctx.account_id, %room_id, %event_id, %from_device, error = %err,
                    "failed to initiate to-device response to self-verification room request"
                );
                return;
            }
        }
    } else {
        // SDK has no request and it isn't from our own user — the expected
        // first-delivery race for a cross-user request (the peer's device keys
        // aren't loaded yet). Return *without* recording the event id, but hold
        // the room subscribed for the request lifetime so redelivery is not
        // bounded by the short candidate-invite TTL.
        ctx.rooms
            .add_pending_request(ctx.account_id, room_id.clone());
        tracing::debug!(
            account_id = %ctx.account_id, %room_id, %event_id, sender = %ev.sender,
            "no SDK verification request for room flow yet; awaiting re-delivery"
        );
        return;
    };

    let target_user_id = request.other_user_id().to_string();
    let request_room_id = request.room_id().map(ToOwned::to_owned);
    let target_device_id = request.is_self_verification().then(|| from_device.clone());
    let registry_key = (ctx.account_id, request.flow_id().to_owned());
    let driver_cancel = ctx.cancel.child_token();

    {
        let mut reg = ctx.registry.lock().expect("flow registry poisoned");
        if reg.contains_key(&registry_key) {
            return;
        }
        reg.insert(
            registry_key.clone(),
            FlowEntry {
                request: request.clone(),
                sas: None,
                target_user_id: target_user_id.clone(),
                target_device_id: target_device_id.clone(),
                room_id: request_room_id,
                terminal: None,
                terminal_at: None,
                driver_cancel: driver_cancel.clone(),
                driver_handle: None,
            },
        );
        // Spawn and attach the handle while still holding the lock — see the
        // matching comment in `on_incoming_request` for why the gap matters.
        let handle = ctx.tracker.spawn(drive_request(
            request,
            FlowDriverCtx {
                account_id: ctx.account_id,
                target_user_id: target_user_id.clone(),
                target_device_id: target_device_id.clone(),
                registry: ctx.registry.clone(),
                live_tx: ctx.live_tx.clone(),
                rooms: ctx.rooms.clone(),
                cancel: driver_cancel,
                client: client.clone(),
            },
        ));
        reg.get_mut(&registry_key)
            .expect("just inserted above")
            .driver_handle = Some(handle);
    }

    tracing::info!(
        account_id = %ctx.account_id,
        %room_id,
        %event_id,
        flow_id = %registry_key.1,
        target_user_id = %target_user_id,
        target_device_id = ?target_device_id,
        "registered incoming room verification request"
    );

    // The request resolved and a flow is now registered, so commit the side effects
    // we deliberately deferred past the transient-miss window:
    //   * record the event id, so a re-delivery is deduped (the flow exists now);
    //   * promote the room out of the TTL'd candidate set — it's an active-flow
    //     room kept subscribed via the registry. Done *after* registration so a
    //     recompute racing in between still sees the room in `active_flow_rooms`
    //     and never transiently unsubscribes it.
    ctx.handled_room_events.mark(event_id);
    ctx.rooms.clear_candidate(ctx.account_id, &room_id);

    // Keep this account's active-flow room subscribed (the loop derives it from the
    // registry on the next recompute).
    ctx.rooms.wake(ctx.account_id);
}

struct FlowDriverCtx {
    account_id: Uuid,
    target_user_id: String,
    target_device_id: Option<String>,
    registry: FlowRegistry,
    live_tx: broadcast::Sender<LiveFrame>,
    rooms: VerificationRooms,
    cancel: CancellationToken,
    /// Live SDK client for this account. Used to recover a SAS from the
    /// verification cache after the request reports `Done` (the request object
    /// no longer holds it).
    client: Client,
}

/// Stamp the registry terminal marker and publish the matching frame. Shared by
/// every Done/Cancelled exit so a request-level `Done` cannot be published as
/// verified from one path while another path requires the SAS MAC.
fn finish_flow(ctx: &FlowDriverCtx, key: &(Uuid, String), flow_id: &str, outcome: TerminalOutcome) {
    let (kind, reason) = match &outcome {
        TerminalOutcome::Done => (VerificationFrameKind::Done, None),
        TerminalOutcome::Cancelled(reason) => (VerificationFrameKind::Cancelled, reason.clone()),
    };
    if mark_terminal(&ctx.registry, key, outcome) {
        ctx.rooms.wake(ctx.account_id);
    }
    publish(
        &ctx.live_tx,
        ctx.account_id,
        flow_id,
        &ctx.target_user_id,
        ctx.target_device_id.as_deref(),
        kind,
        None,
        None,
        reason,
    );
}

/// Drive one verification request from its current state through to a
/// `SasVerification`, then hand off to [`drive_sas`]. Publishes the
/// `verification.requested` frame, accepts a peer-initiated request, and starts
/// SAS once ready. [`drive_sas`] keeps watching this request so a peer `start`
/// that wins the spec tie-break replaces the SAS object we drive.
async fn drive_request(request: VerificationRequest, ctx: FlowDriverCtx) {
    let flow_id = request.flow_id().to_owned();
    let key = (ctx.account_id, flow_id.clone());

    tracing::debug!(
        account_id = %ctx.account_id,
        %flow_id,
        target_user_id = %ctx.target_user_id,
        target_device_id = ?ctx.target_device_id,
        we_started = request.we_started(),
        "driving verification request"
    );

    publish(
        &ctx.live_tx,
        ctx.account_id,
        &flow_id,
        &ctx.target_user_id,
        ctx.target_device_id.as_deref(),
        VerificationFrameKind::Requested,
        None,
        None,
        None,
    );

    // Subscribe before sending our accept/start response. Some peers answer
    // quickly enough that creating the stream after the send can miss the
    // transition into ready/SAS and leave the driver waiting forever while the
    // snapshot endpoint reports the flow as ready.
    //
    // Boxed rather than `pin_mut!`ed so the *same* subscription can be handed
    // to `drive_sas` instead of it opening a second one — see the note there.
    let mut changes = Box::pin(request.changes());

    // A peer-initiated request must be accepted to advance. Advertise SAS only —
    // never the SDK default set (which includes QR) — so the peer can't steer the
    // flow to a method this driver doesn't implement.
    if !request.we_started() {
        match tokio::time::timeout(DRIVER_SEND_TIMEOUT, request.accept_with_methods(sas_only()))
            .await
        {
            Ok(Ok(())) => {
                tracing::debug!(
                    account_id = %ctx.account_id,
                    %flow_id,
                    "accepted incoming verification request"
                );
            }
            Ok(Err(err)) => {
                tracing::warn!(account_id = %ctx.account_id, %flow_id, error = %err, "failed to accept verification request");
            }
            Err(_) => {
                tracing::warn!(
                    account_id = %ctx.account_id,
                    %flow_id,
                    timeout_secs = DRIVER_SEND_TIMEOUT.as_secs(),
                    "timed out accepting verification request"
                );
                cancel_verification_best_effort(
                    ctx.account_id,
                    &flow_id,
                    "request",
                    request.cancel(),
                )
                .await;
                remove_flow(&ctx.registry, &key);
                return;
            }
        }
    }

    let mut pending_state = Some(request.state());

    let sas = loop {
        let state = if let Some(state) = pending_state.take() {
            state
        } else {
            tokio::select! {
                _ = ctx.cancel.cancelled() => {
                    // Torn down mid-flight (typically the account logging out). Best-
                    // effort cancel upstream, then drop the entry — it has no terminal
                    // outcome to retain, and leaving it would leak (the TTL sweep only
                    // reclaims entries with `terminal_at` set).
                    cancel_verification_best_effort(
                        ctx.account_id,
                        &flow_id,
                        "request",
                        request.cancel(),
                    )
                    .await;
                    remove_flow(&ctx.registry, &key);
                    return;
                }
                next = changes.next() => match next {
                None => {
                    remove_flow(&ctx.registry, &key);
                    return;
                }
                    Some(state) => state,
                },
            }
        };

        match state {
            VerificationRequestState::Created { .. }
            | VerificationRequestState::Requested { .. } => {
                tracing::debug!(
                    account_id = %ctx.account_id,
                    %flow_id,
                    state = request_state_label(&state),
                    "verification request state changed"
                );
            }
            VerificationRequestState::Ready { .. } => {
                tracing::debug!(
                    account_id = %ctx.account_id,
                    %flow_id,
                    state = request_state_label(&state),
                    we_started = request.we_started(),
                    "verification request state changed"
                );
                // Matrix allows either side to move a ready request into a
                // concrete SAS flow. Waiting for only the requester to send
                // `m.key.verification.start` deadlocks with clients that expect
                // the responder to start after advertising `m.sas.v1`.
                tracing::info!(
                    account_id = %ctx.account_id,
                    %flow_id,
                    "verification request ready; starting SAS"
                );
                match tokio::time::timeout(DRIVER_SEND_TIMEOUT, request.start_sas()).await {
                    Ok(Ok(Some(sas))) => {
                        tracing::info!(
                            account_id = %ctx.account_id,
                            %flow_id,
                            "started SAS verification"
                        );
                        break sas;
                    }
                    Ok(Ok(None)) => {
                        tracing::info!(
                            account_id = %ctx.account_id,
                            %flow_id,
                            "request.start_sas returned no SAS object"
                        );
                    }
                    Ok(Err(err)) => tracing::warn!(
                        account_id = %ctx.account_id, %flow_id, error = %err,
                        "failed to start SAS"
                    ),
                    Err(_) => {
                        tracing::warn!(
                            account_id = %ctx.account_id,
                            %flow_id,
                            timeout_secs = DRIVER_SEND_TIMEOUT.as_secs(),
                            "timed out starting SAS"
                        );
                        cancel_verification_best_effort(
                            ctx.account_id,
                            &flow_id,
                            "request",
                            request.cancel(),
                        )
                        .await;
                        remove_flow(&ctx.registry, &key);
                        return;
                    }
                }
            }
            VerificationRequestState::Transitioned { verification } => {
                let sas = verification.sas();
                tracing::debug!(
                    account_id = %ctx.account_id,
                    %flow_id,
                    state = "transitioned",
                    has_sas = sas.is_some(),
                    "verification request state changed"
                );
                match sas {
                    Some(sas) => {
                        tracing::info!(
                            account_id = %ctx.account_id,
                            %flow_id,
                            "verification transitioned to SAS"
                        );
                        break sas;
                    }
                    // QR (or any non-SAS) isn't supported here — cancel.
                    None => {
                        cancel_verification_best_effort(
                            ctx.account_id,
                            &flow_id,
                            "request",
                            request.cancel(),
                        )
                        .await;
                    }
                }
            }
            VerificationRequestState::Done => {
                // A coalesced `changes()` burst can skip `Transitioned` and
                // land on request-level `Done`. `request.state()` is then
                // `Done`, so `sas_from_request` cannot fire — recover from the
                // registry (already adopted) or the SDK cache (still holds the
                // SAS). Otherwise this is the peer's `m.key.verification.done`
                // with no SAS object, and publishing `Done` would let clients
                // render "device verified" with zero MAC exchange.
                if let Some(sas) = sas_after_request_done(
                    &ctx.registry,
                    &key,
                    &ctx.client,
                    &ctx.target_user_id,
                    &flow_id,
                )
                .await
                {
                    tracing::info!(
                        account_id = %ctx.account_id,
                        %flow_id,
                        sas_state = sas_state_label(&sas.state()),
                        "verification request reported done; recovering SAS"
                    );
                    break sas;
                }
                tracing::info!(
                    account_id = %ctx.account_id,
                    %flow_id,
                    "verification request completed before SAS; not treating as verified"
                );
                finish_flow(
                    &ctx,
                    &key,
                    &flow_id,
                    TerminalOutcome::Cancelled(Some(
                        "request completed before SAS verification".to_owned(),
                    )),
                );
                return;
            }
            VerificationRequestState::Cancelled(info) => {
                let reason = info.reason().to_owned();
                tracing::info!(
                    account_id = %ctx.account_id,
                    %flow_id,
                    reason = %reason,
                    "verification request cancelled"
                );
                finish_flow(
                    &ctx,
                    &key,
                    &flow_id,
                    TerminalOutcome::Cancelled(Some(reason)),
                );
                return;
            }
        }
    };

    // Stash + protocol-level accept, then drive. `drive_sas` keeps watching the
    // request: both sides are allowed to send `m.key.verification.start`, and
    // matrix-rust-sdk may replace the SAS object we just obtained with the
    // lexicographic winner. Confirm/cancel read `entry.sas`, so the registry
    // must follow that replacement too.
    if !adopt_sas(&sas, &flow_id, &key, &ctx).await {
        return;
    }
    drive_sas(request, sas, flow_id, ctx, changes).await;
}

/// Stash `sas` on the registry entry and send the protocol-level accept (a
/// no-op for the side that sent `m.key.verification.start`).
///
/// Returns `false` if the accept timed out and this flow was torn down.
async fn adopt_sas(
    sas: &SasVerification,
    flow_id: &str,
    key: &(Uuid, String),
    ctx: &FlowDriverCtx,
) -> bool {
    if let Some(entry) = ctx
        .registry
        .lock()
        .expect("flow registry poisoned")
        .get_mut(key)
    {
        entry.sas = Some(sas.clone());
    }
    match tokio::time::timeout(DRIVER_SEND_TIMEOUT, sas.accept()).await {
        Ok(Ok(())) => {
            tracing::debug!(
                account_id = %ctx.account_id,
                %flow_id,
                state = sas_state_label(&sas.state()),
                "accepted SAS verification"
            );
            true
        }
        Ok(Err(err)) => {
            tracing::warn!(
                account_id = %ctx.account_id,
                %flow_id,
                error = %err,
                "failed to accept SAS"
            );
            true
        }
        Err(_) => {
            tracing::warn!(
                account_id = %ctx.account_id,
                %flow_id,
                timeout_secs = DRIVER_SEND_TIMEOUT.as_secs(),
                "timed out accepting SAS"
            );
            cancel_verification_best_effort(ctx.account_id, flow_id, "sas", sas.cancel()).await;
            remove_flow(&ctx.registry, key);
            false
        }
    }
}

/// Drive a SAS verification to a terminal state, publishing the `sas`, `done`,
/// and `cancelled` frames and keeping the registry's terminal marker current.
///
/// Also watches the parent [`VerificationRequest`]: Matrix lets either side send
/// `m.key.verification.start`, and two axons (or axon + Element X) both will.
/// matrix-rust-sdk then keeps the lexicographically smaller device-id's SAS and
/// replaces the other. If we kept driving the object `start_sas()` returned, we
/// never `accept()` the winner, keys never exchange, and both UIs sit on
/// "waiting". A `Transitioned` snapshot with a different [`SasState`] is that
/// replacement — adopt it, accept it, drive it.
///
/// `request_changes` is the caller's subscription, handed over rather than
/// re-opened here. `SharedObservable::subscribe` seeds at the current version
/// and never replays, so a stream opened at this point would silently miss
/// every transition published during `start_sas()` and the `accept()` send —
/// which is exactly the window the dual-start replacement lands in, and the
/// deadlock above would survive unfixed in its likeliest case.
async fn drive_sas(
    request: VerificationRequest,
    mut sas: SasVerification,
    flow_id: String,
    ctx: FlowDriverCtx,
    mut request_changes: impl Stream<Item = VerificationRequestState> + Unpin,
) {
    let key = (ctx.account_id, flow_id.clone());

    loop {
        let sas_changes = sas.changes();
        pin_mut!(sas_changes);
        let mut pending_state = Some(sas.state());

        let new_sas = loop {
            let state = if let Some(state) = pending_state.take() {
                state
            } else {
                tokio::select! {
                    _ = ctx.cancel.cancelled() => {
                        // See drive_request: drop the entry rather than leak a
                        // non-terminal one the TTL sweep would never reclaim.
                        cancel_verification_best_effort(
                            ctx.account_id,
                            &flow_id,
                            "sas",
                            sas.cancel(),
                        )
                        .await;
                        remove_flow(&ctx.registry, &key);
                        return;
                    }
                    next = request_changes.next() => match next {
                        None => {
                            remove_flow(&ctx.registry, &key);
                            return;
                        }
                        Some(VerificationRequestState::Transitioned { verification }) => {
                            match verification.sas() {
                                Some(new_sas)
                                    if should_follow_sas_replacement(
                                        &sas.state(),
                                        &new_sas.state(),
                                    ) =>
                                {
                                    break new_sas;
                                }
                                Some(_) => continue,
                                // QR (or any non-SAS) isn't supported — cancel
                                // and wait for the resulting Cancelled.
                                None => {
                                    cancel_verification_best_effort(
                                        ctx.account_id,
                                        &flow_id,
                                        "request",
                                        request.cancel(),
                                    )
                                    .await;
                                    continue;
                                }
                            }
                        }
                        Some(VerificationRequestState::Cancelled(info)) => {
                            let reason = info.reason().to_owned();
                            tracing::info!(
                                account_id = %ctx.account_id,
                                %flow_id,
                                reason = %reason,
                                "verification request cancelled"
                            );
                            finish_flow(
                                &ctx,
                                &key,
                                &flow_id,
                                TerminalOutcome::Cancelled(Some(reason)),
                            );
                            return;
                        }
                        Some(VerificationRequestState::Done) => {
                            if !request_done_is_verified(sas.is_done()) {
                                tracing::info!(
                                    account_id = %ctx.account_id,
                                    %flow_id,
                                    sas_state = sas_state_label(&sas.state()),
                                    "verification request reported done before the SAS \
                                     verified; waiting for the SAS verdict"
                                );
                                continue;
                            }
                            tracing::info!(
                                account_id = %ctx.account_id,
                                %flow_id,
                                "verification request completed"
                            );
                            finish_flow(&ctx, &key, &flow_id, TerminalOutcome::Done);
                            return;
                        }
                        Some(_) => continue,
                    },
                    next = sas_changes.next() => match next {
                        None => {
                            remove_flow(&ctx.registry, &key);
                            return;
                        }
                        Some(state) => state,
                    },
                }
            };

            match state {
                SasState::Created { .. }
                | SasState::Started { .. }
                | SasState::Accepted { .. }
                | SasState::Confirmed => {
                    tracing::debug!(
                        account_id = %ctx.account_id,
                        %flow_id,
                        state = sas_state_label(&state),
                        "SAS verification state changed"
                    );
                }
                SasState::KeysExchanged { .. } => {
                    let emoji = emoji_pairs(&sas);
                    let decimals = sas.decimals();
                    tracing::info!(
                        account_id = %ctx.account_id,
                        %flow_id,
                        has_emoji = emoji.as_ref().is_some_and(|items| !items.is_empty()),
                        has_decimals = decimals.is_some(),
                        "SAS keys exchanged"
                    );
                    publish(
                        &ctx.live_tx,
                        ctx.account_id,
                        &flow_id,
                        &ctx.target_user_id,
                        ctx.target_device_id.as_deref(),
                        VerificationFrameKind::Sas,
                        emoji,
                        decimals,
                        None,
                    );
                }
                SasState::Done { .. } => {
                    tracing::info!(
                        account_id = %ctx.account_id,
                        %flow_id,
                        "SAS verification completed"
                    );
                    finish_flow(&ctx, &key, &flow_id, TerminalOutcome::Done);
                    return;
                }
                SasState::Cancelled(info) => {
                    let reason = info.reason().to_owned();
                    tracing::info!(
                        account_id = %ctx.account_id,
                        %flow_id,
                        reason = %reason,
                        "SAS verification cancelled"
                    );
                    finish_flow(
                        &ctx,
                        &key,
                        &flow_id,
                        TerminalOutcome::Cancelled(Some(reason)),
                    );
                    return;
                }
            }
        };

        tracing::info!(
            account_id = %ctx.account_id,
            %flow_id,
            previous_state = sas_state_label(&sas.state()),
            new_state = sas_state_label(&new_sas.state()),
            "adopting replacement SAS after dual start"
        );
        if !adopt_sas(&new_sas, &flow_id, &key, &ctx).await {
            return;
        }
        sas = new_sas;
    }
}

/// Does a request-level `Done` prove the SAS verified?
///
/// Only if our own SAS says so. [`VerificationRequestState::Done`] is reached on
/// the peer's `m.key.verification.done` and nothing else: `receive_done` checks
/// only that the sender is the other user, and `into_done` throws the content
/// away without ever consulting the SAS. `SasState::Done` is the authoritative
/// state — it alone carries `verified_devices()` / `verified_identities()`, and
/// it alone is reachable only after the MAC exchange.
///
/// Publishing a `done` frame on the request-level signal would let a buggy or
/// hostile peer make clients render "device verified" with no MAC ever checked.
/// Peers do legitimately send `m.key.verification.done` before our own MAC
/// processing finishes, so the answer to a premature one is to keep driving the
/// SAS, not to cancel — its own terminal follows a moment later.
fn request_done_is_verified(sas_is_done: bool) -> bool {
    sas_is_done
}

/// Dual-start: the SDK replaces our `start_sas()` object with the peer's when
/// the peer's device id wins the spec tie-break. Those two objects do not share
/// a state machine. A different [`SasState`] label is a replacement; so is the
/// same pre-emoji label (`created` / `started`), because both SAS objects sit
/// there until accept/key.
fn should_follow_sas_replacement(current: &SasState, incoming: &SasState) -> bool {
    should_follow_sas_replacement_labels(sas_state_label(current), sas_state_label(incoming))
}

fn should_follow_sas_replacement_labels(current: &str, incoming: &str) -> bool {
    if current != incoming {
        return true;
    }
    // Dual-start: the winner's SAS is a *new* object that can sit in the same
    // pre-emoji label (`created` / `started`) as the loser we are driving.
    matches!(current, "created" | "started")
}

/// The concrete device-verification backend. Cheap to clone (all handles); built
/// by [`SyncEngine::verification`](crate::SyncEngine::verification) and adapted
/// onto the API's verification port by `axon-server`.
#[derive(Clone)]
pub struct VerificationEngine {
    store: Store,
    manager: ClientManager,
    registry: FlowRegistry,
    live_tx: broadcast::Sender<LiveFrame>,
    tracker: TaskTracker,
    cancel: CancellationToken,
    /// Per-identity lifecycle locks, shared with [`AccountLifecycle`]. `start`
    /// takes the same lock the login/logout/delete verbs take, so a flow can't be
    /// started against an account that a concurrent logout/delete is severing, and
    /// the login activation window (row `active` but the supervised task not yet
    /// registered) is closed — login holds the lock across both.
    locks: IdentityLocks,
    /// Per-account verification room subscriptions, shared with each account's
    /// sync loop. A cross-user `start` adds its DM here and wakes the loop so the
    /// flow's room events are delivered (ADR 0040).
    rooms: VerificationRooms,
}

impl VerificationEngine {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        store: Store,
        manager: ClientManager,
        registry: FlowRegistry,
        live_tx: broadcast::Sender<LiveFrame>,
        tracker: TaskTracker,
        cancel: CancellationToken,
        locks: IdentityLocks,
        rooms: VerificationRooms,
    ) -> Self {
        Self {
            store,
            manager,
            registry,
            live_tx,
            tracker,
            cancel,
            locks,
            rooms,
        }
    }

    /// Take the per-identity lifecycle lock for `account_id` and require the
    /// account still be `active` under it, returning the held guard for the caller
    /// to keep alive across its client operation.
    ///
    /// This is the serialization point for the verbs that drive the live SDK flow
    /// (`confirm`/`cancel`): login/logout/delete hold this *same* lock across their
    /// whole teardown (deactivate → sever session → cancel in-flight flows), so
    /// once this returns the account can't be severed until the guard drops — and a
    /// already-severed account is rejected here, never allowed to send another MAC
    /// or cancel after teardown has begun (which would continue a trust flow on a
    /// logged-out device whose `verified` flag was just reset).
    async fn lock_active(&self, account_id: Uuid) -> Result<OwnedMutexGuard<()>, VerifyError> {
        // Read once (unlocked) to resolve the identity the lock is keyed by.
        let account = self
            .store
            .get_account(account_id)
            .await
            .map_err(|e| VerifyError::Store(e.to_string()))?
            .ok_or(VerifyError::NotFound(account_id))?;
        let lock = lock_for(&self.locks, &account.user_id, &account.homeserver_url);
        let guard = lock.lock_owned().await;
        // Re-read under the lock: a verb that ran while we waited may have changed
        // the account's lifecycle state.
        let account = self
            .store
            .get_account(account_id)
            .await
            .map_err(|e| VerifyError::Store(e.to_string()))?
            .ok_or(VerifyError::NotFound(account_id))?;
        match account.state {
            AccountState::Active => Ok(guard),
            AccountState::Deactivated => Err(VerifyError::NotActive(account_id)),
            AccountState::Deleting => Err(VerifyError::BeingDeleted(account_id)),
        }
    }

    /// Gate an already-read account on `active` and return its live client and
    /// parsed user id. Called under the identity lock (see [`start`](Self::start))
    /// so the state it gates on can't change under it.
    async fn active_client(&self, account: &Account) -> Result<(Client, OwnedUserId), VerifyError> {
        match account.state {
            AccountState::Active => {}
            AccountState::Deactivated => return Err(VerifyError::NotActive(account.account_id)),
            AccountState::Deleting => return Err(VerifyError::BeingDeleted(account.account_id)),
        }
        let client = self
            .manager
            .get_or_connect(account.account_id)
            .await
            .map_err(|e| match e {
                GatewayError::UnknownAccount(id) => VerifyError::NotFound(id),
                GatewayError::AccountNotActive(id) => VerifyError::NotActive(id),
                other => VerifyError::Upstream(other.to_string()),
            })?;
        let user_id = account.user_id.parse::<OwnedUserId>().map_err(|_| {
            VerifyError::Upstream(format!("invalid stored user id: {}", account.user_id))
        })?;
        Ok((client, user_id))
    }

    /// Start a SAS verification for `account_id`, returning the new flow's id. A
    /// `device_id` target is self-verification of the account's own device; a
    /// `target_user` target is cross-user verification (ADR 0040).
    pub async fn start(
        &self,
        account_id: Uuid,
        target_user: Option<&str>,
        device_id: Option<&str>,
    ) -> Result<String, VerifyError> {
        sweep_expired(&self.registry);

        // Read once (unlocked) to resolve the identity the lock is keyed by — the
        // `(user_id, homeserver_url)` pair never changes for an account id.
        let account = self
            .store
            .get_account(account_id)
            .await
            .map_err(|e| VerifyError::Store(e.to_string()))?
            .ok_or(VerifyError::NotFound(account_id))?;

        // Serialize against login/logout/delete on this identity by taking the
        // *same* lock those verbs hold, held across the whole critical section so a
        // concurrent teardown can't deactivate/evict the client under us, and so
        // the login activation window (row `active` but task not yet registered) is
        // closed — login holds this lock across both the activation and the task
        // spawn.
        let lock = lock_for(&self.locks, &account.user_id, &account.homeserver_url);
        let _guard = lock.lock().await;

        // Re-read under the lock: a verb that ran while we waited may have changed
        // the account's lifecycle state.
        let account = self
            .store
            .get_account(account_id)
            .await
            .map_err(|e| VerifyError::Store(e.to_string()))?
            .ok_or(VerifyError::NotFound(account_id))?;
        let (client, user_id) = self.active_client(&account).await?;

        // Build the SDK request and the flow's metadata from whichever target was
        // named. SAS only — never the SDK default method set (which advertises QR).
        let (request, target_user_id, target_device_id, room_id) = match (device_id, target_user) {
            (Some(_), Some(_)) => return Err(VerifyError::AmbiguousTarget),
            // Self-verification of one of our own devices (to-device transport).
            (Some(device_id), None) => {
                let device_id_owned: OwnedDeviceId = device_id.into();
                let device = client
                    .encryption()
                    .get_device(&user_id, &device_id_owned)
                    .await
                    .map_err(|e| VerifyError::Upstream(e.to_string()))?
                    .ok_or_else(|| VerifyError::UnknownDevice(device_id.to_owned()))?;
                let request = device
                    .request_verification_with_methods(sas_only())
                    .await
                    .map_err(|e| VerifyError::Upstream(e.to_string()))?;
                (
                    request,
                    user_id.to_string(),
                    Some(device_id.to_owned()),
                    None,
                )
            }
            // Cross-user verification of another user's identity over a DM (ADR
            // 0040). The SDK finds or creates the DM and sends the room event; the
            // returned request's `room_id` is the DM the loop must subscribe so the
            // peer's ready/accept/mac events are delivered.
            (None, Some(target_user)) => {
                let peer: OwnedUserId = target_user
                    .parse()
                    .map_err(|_| VerifyError::UnknownUser(target_user.to_owned()))?;
                let identity = client
                    .encryption()
                    .get_user_identity(&peer)
                    .await
                    .map_err(|e| VerifyError::Upstream(e.to_string()))?
                    .ok_or_else(|| VerifyError::UnknownUser(target_user.to_owned()))?;
                let request = identity
                    .request_verification_with_methods(sas_only())
                    .await
                    .map_err(|e| VerifyError::Upstream(e.to_string()))?;
                let room_id = request.room_id().map(ToOwned::to_owned);
                (request, peer.to_string(), None, room_id)
            }
            (None, None) => return Err(VerifyError::NoTarget),
        };
        let flow_id = request.flow_id().to_owned();
        tracing::info!(
            account_id = %account_id,
            %flow_id,
            target_user_id = %target_user_id,
            target_device_id = ?target_device_id,
            is_room_flow = room_id.is_some(),
            room_id = ?room_id,
            "started outgoing verification flow"
        );

        // The driver runs under a child of the engine token (so engine shutdown
        // cascades to it) and is also reachable for cancellation via the registry,
        // so a sync-run teardown for this account stops it (see
        // [`cancel_account_flows`]) — it never outlives the SDK client it drives.
        let driver_cancel = self.cancel.child_token();
        let is_room_flow = room_id.is_some();
        let key = (account_id, flow_id.clone());
        {
            let mut reg = self.registry.lock().expect("flow registry poisoned");
            reg.insert(
                key.clone(),
                FlowEntry {
                    request: request.clone(),
                    sas: None,
                    target_user_id: target_user_id.clone(),
                    target_device_id: target_device_id.clone(),
                    room_id,
                    terminal: None,
                    terminal_at: None,
                    driver_cancel: driver_cancel.clone(),
                    driver_handle: None,
                },
            );
            // Spawn and attach the handle while still holding the lock — see the
            // matching comment in `on_incoming_request` for why the gap matters.
            let handle = self.tracker.spawn(drive_request(
                request,
                FlowDriverCtx {
                    account_id,
                    target_user_id,
                    target_device_id,
                    registry: self.registry.clone(),
                    live_tx: self.live_tx.clone(),
                    rooms: self.rooms.clone(),
                    cancel: driver_cancel,
                    client,
                },
            ));
            reg.get_mut(&key)
                .expect("just inserted above")
                .driver_handle = Some(handle);
        }

        // A cross-user flow runs over a DM the sliding-sync loop must subscribe so
        // the peer's responses are delivered; wake it to pick up the new active-flow
        // room (which it derives from the registry).
        if is_room_flow {
            self.rooms.wake(account_id);
        }

        Ok(flow_id)
    }

    /// List the account's currently-tracked flows (live + recently-terminal).
    pub async fn list(&self, account_id: Uuid) -> Result<Vec<FlowState>, VerifyError> {
        sweep_expired(&self.registry);
        if self
            .store
            .get_account(account_id)
            .await
            .map_err(|e| VerifyError::Store(e.to_string()))?
            .is_none()
        {
            return Err(VerifyError::NotFound(account_id));
        }
        let reg = self.registry.lock().expect("flow registry poisoned");
        Ok(reg
            .iter()
            .filter(|((aid, _), _)| *aid == account_id)
            .map(|(_, entry)| snapshot(entry))
            .collect())
    }

    /// Read one flow's replayable state.
    pub async fn get(&self, account_id: Uuid, flow_id: &str) -> Result<FlowState, VerifyError> {
        sweep_expired(&self.registry);
        // Gate on account existence (like `list`) so a flow that outlived its
        // account — e.g. a terminal entry still inside its grace TTL when the
        // account was deleted — reads as gone, not as stale state.
        if self
            .store
            .get_account(account_id)
            .await
            .map_err(|e| VerifyError::Store(e.to_string()))?
            .is_none()
        {
            return Err(VerifyError::NotFound(account_id));
        }
        let reg = self.registry.lock().expect("flow registry poisoned");
        reg.get(&(account_id, flow_id.to_owned()))
            .map(snapshot)
            .ok_or_else(|| VerifyError::FlowNotFound(flow_id.to_owned()))
    }

    /// Confirm that the SAS matches. Requires the flow to have reached the
    /// key-exchanged stage; confirming an already-confirmed or already-done flow is
    /// an idempotent success, but confirming a cancelled flow is an error (it must
    /// never be reported as a successful confirmation).
    pub async fn confirm(&self, account_id: Uuid, flow_id: &str) -> Result<(), VerifyError> {
        // Serialize against login/logout/delete and reject a severed account before
        // touching the live SAS object: `confirm` sends this side's MAC, a
        // trust-advancing client op that must not run after teardown has begun.
        // Held across the whole verb, including `sas.confirm()` below.
        let _guard = self.lock_active(account_id).await?;
        let (sas, target_user_id, target_device_id) = {
            let reg = self.registry.lock().expect("flow registry poisoned");
            let entry = reg
                .get(&(account_id, flow_id.to_owned()))
                .ok_or_else(|| VerifyError::FlowNotFound(flow_id.to_owned()))?;
            match &entry.terminal {
                Some(TerminalOutcome::Done) => return Ok(()),
                Some(TerminalOutcome::Cancelled(reason)) => {
                    return Err(cancelled_err(reason.as_deref()))
                }
                None => {}
            }
            (
                entry
                    .sas
                    .clone()
                    .ok_or_else(|| VerifyError::WrongStage("SAS not yet started".to_owned()))?,
                entry.target_user_id.clone(),
                entry.target_device_id.clone(),
            )
        };

        // Presence of the SAS object does not mean it can be confirmed: the driver
        // stashes it before keys are exchanged, and matrix-rust-sdk treats
        // `confirm()` before the key exchange as a silent no-op — which would return
        // success with no MAC actually sent. Gate on the live SAS state.
        match sas.state() {
            SasState::Created { .. } | SasState::Started { .. } | SasState::Accepted { .. } => {
                return Err(VerifyError::WrongStage(
                    "SAS keys not yet exchanged; nothing to confirm".to_owned(),
                ));
            }
            // Already confirmed our side (awaiting the peer's MAC) or fully done —
            // a client retry. Idempotent success.
            SasState::Confirmed | SasState::Done { .. } => return Ok(()),
            SasState::Cancelled(info) => return Err(cancelled_err(Some(info.reason()))),
            SasState::KeysExchanged { .. } => {}
        }

        if let Err(err) = sas.confirm().await {
            // The flow may have raced to terminal between the lock release above and
            // this call: a done flow is idempotent success, a cancelled one is not.
            // This intentionally mirrors the `drive_sas` Done handler. Both clones
            // share one SDK state machine, so a completion can publish Done from
            // both paths; that duplicate frame is accepted to avoid making either
            // path stale when the other misses a race.
            if sas.is_done() {
                tracing::info!(
                    account_id = %account_id,
                    %flow_id,
                    "SAS verification completed"
                );
                let key = (account_id, flow_id.to_owned());
                if mark_terminal(&self.registry, &key, TerminalOutcome::Done) {
                    self.rooms.wake(account_id);
                }
                publish(
                    &self.live_tx,
                    account_id,
                    flow_id,
                    &target_user_id,
                    target_device_id.as_deref(),
                    VerificationFrameKind::Done,
                    None,
                    None,
                    None,
                );
                return Ok(());
            }
            if sas.is_cancelled() {
                return Err(cancelled_err(None));
            }
            return Err(VerifyError::Upstream(err.to_string()));
        }
        tracing::info!(
            account_id = %account_id,
            %flow_id,
            state = sas_state_label(&sas.state()),
            "SAS verification confirmed locally"
        );
        // See the race comment above: `drive_sas` may also observe this Done
        // transition and publish the same terminal frame.
        if sas.is_done() {
            tracing::info!(
                account_id = %account_id,
                %flow_id,
                "SAS verification completed"
            );
            let key = (account_id, flow_id.to_owned());
            if mark_terminal(&self.registry, &key, TerminalOutcome::Done) {
                self.rooms.wake(account_id);
            }
            publish(
                &self.live_tx,
                account_id,
                flow_id,
                &target_user_id,
                target_device_id.as_deref(),
                VerificationFrameKind::Done,
                None,
                None,
                None,
            );
        }
        Ok(())
    }

    /// Cancel the flow. Idempotent — cancelling a terminal flow is a no-op, and an
    /// already-terminal SDK error is swallowed.
    pub async fn cancel(&self, account_id: Uuid, flow_id: &str) -> Result<(), VerifyError> {
        // Same lifecycle serialization as `confirm`: `cancel` drives the live SDK
        // object (a to-device send), so it must not race a concurrent teardown
        // either; a severed account is rejected rather than reaching an evicted
        // client. Held across the cancel send below.
        let _guard = self.lock_active(account_id).await?;
        let (sas, request) = {
            let reg = self.registry.lock().expect("flow registry poisoned");
            let entry = reg
                .get(&(account_id, flow_id.to_owned()))
                .ok_or_else(|| VerifyError::FlowNotFound(flow_id.to_owned()))?;
            if entry.terminal.is_some() {
                return Ok(());
            }
            (entry.sas.clone(), entry.request.clone())
        };
        // Cancel via the SAS object if there is one, else the request. A send
        // failure is only safe to swallow when the flow is *already* terminal (it
        // raced to done/cancelled under us — cancel is idempotent); a genuine
        // homeserver/network failure must surface as `Upstream`, or the caller would
        // believe it cancelled while the peer keeps driving the exchange.
        match sas {
            Some(sas) => {
                if let Err(err) = sas.cancel().await {
                    if sas.is_done() || sas.is_cancelled() {
                        return Ok(());
                    }
                    return Err(VerifyError::Upstream(err.to_string()));
                }
            }
            None => {
                if let Err(err) = request.cancel().await {
                    if request.is_done() || request.is_cancelled() {
                        return Ok(());
                    }
                    return Err(VerifyError::Upstream(err.to_string()));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::ClientManager;
    use axon_core::SyncConfig;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Dual-start: a peer `m.key.verification.start` that wins the tie-break
    /// produces a *new* SAS object. It may already be `started` while we are
    /// also `started` on the loser — same label, different object. After emoji
    /// (`keys_exchanged` / `confirmed`) a matching label is the SAS we already
    /// drive; do not tear down its `changes()` stream.
    #[test]
    fn follow_sas_replacement_when_state_labels_differ() {
        assert!(should_follow_sas_replacement_labels("started", "created"));
        assert!(should_follow_sas_replacement_labels("started", "accepted"));
        assert!(should_follow_sas_replacement_labels("started", "started"));
        assert!(should_follow_sas_replacement_labels("created", "created"));
        assert!(!should_follow_sas_replacement_labels(
            "keys_exchanged",
            "keys_exchanged"
        ));
        assert!(!should_follow_sas_replacement_labels(
            "confirmed",
            "confirmed"
        ));
    }

    /// A request-level `Done` is the peer's word, not proof. `receive_done`
    /// authenticates nothing but the sender and `into_done` ignores the content,
    /// so a peer that sends `m.key.verification.done` while our SAS is still at
    /// `keys_exchanged` / `confirmed` must not be able to make us publish a
    /// `done` frame — clients render that as "device verified" with no MAC ever
    /// checked. Only `SasState::Done` carries the verified devices.
    #[test]
    fn request_done_is_proof_only_when_the_sas_agrees() {
        assert!(request_done_is_verified(true));
        assert!(
            !request_done_is_verified(false),
            "a peer's m.key.verification.done must not stand in for the SAS verdict"
        );
    }

    /// Pre-SAS request-level `Done` (drive_request before any SAS object, and
    /// snapshot's request-only fallback) is the same signal as above — not
    /// verification. The poll path would otherwise report `FlowStage::Done` for
    /// a flow that never exchanged MACs.
    #[test]
    fn request_done_without_sas_is_not_a_verified_snapshot() {
        assert_eq!(
            flow_stage_from_request_without_sas(&VerificationRequestState::Done),
            FlowStage::Cancelled
        );
        assert!(
            !request_done_is_verified(false),
            "drive_request must not publish Done when no SAS exists to agree"
        );
    }

    /// The request-`Done` recovery path looks in the registry before the SDK
    /// cache. An unknown flow has no stashed SAS, so that first lookup is
    /// `None` and the driver must not treat request-level `Done` as verified.
    #[test]
    fn registry_sas_is_none_when_the_flow_is_unknown() {
        let registry = new_registry();
        assert!(
            registry_sas(&registry, &(Uuid::nil(), "flow".to_owned())).is_none(),
            "drive_request's Done recovery must not invent a SAS"
        );
    }

    /// `cancelled_err` is a pure mapping — no DB needed. It must carry the reason
    /// when there is one and never read as a success.
    #[test]
    fn cancelled_err_carries_reason_and_is_wrong_stage() {
        match cancelled_err(Some("m.mismatched_sas")) {
            VerifyError::WrongStage(msg) => {
                assert!(msg.contains("m.mismatched_sas"), "reason missing: {msg}");
            }
            other => panic!("expected WrongStage, got {other:?}"),
        }
        match cancelled_err(None) {
            VerifyError::WrongStage(msg) => assert!(msg.contains("cancelled")),
            other => panic!("expected WrongStage, got {other:?}"),
        }
    }

    /// `join_or_abort_drivers` must actually wait for a driver that exits
    /// promptly rather than aborting it out from under itself — a driver mid-way
    /// through its own best-effort upstream cancel (see `drive_request`) should be
    /// allowed to finish that within the timeout, not be killed pre-emptively.
    #[tokio::test]
    async fn join_or_abort_drivers_waits_for_a_task_that_exits_promptly() {
        let ran_to_completion = Arc::new(AtomicBool::new(false));
        let flag = ran_to_completion.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            flag.store(true, Ordering::SeqCst);
        });

        join_or_abort_drivers(
            &new_registry(),
            Uuid::new_v4(),
            vec![("flow".to_owned(), handle)],
        )
        .await;

        assert!(
            ran_to_completion.load(Ordering::SeqCst),
            "a driver that exits well within DRIVER_JOIN_TIMEOUT must be allowed to finish, not be aborted"
        );
    }

    /// Regression for the fd-exhaustion leak (GH #242): a driver that ignores its
    /// cancellation token entirely (wedged, or non-yielding code) must still be
    /// forced out via `abort()` once `DRIVER_JOIN_TIMEOUT` elapses — otherwise it
    /// keeps its `Client` clone (and the crypto store's whole connection pool
    /// behind it) alive forever, exactly the leak this function exists to close.
    #[tokio::test]
    async fn join_or_abort_drivers_aborts_a_task_that_ignores_cancellation() {
        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        let abort_handle = handle.abort_handle();

        join_or_abort_drivers(
            &new_registry(),
            Uuid::new_v4(),
            vec![("flow".to_owned(), handle)],
        )
        .await;
        // The abort lands at the task's next await point; give the scheduler one
        // more tick to actually observe it before asserting.
        tokio::task::yield_now().await;

        assert!(
            abort_handle.is_finished(),
            "a driver that ignores cancellation must be aborted once DRIVER_JOIN_TIMEOUT elapses"
        );
    }

    /// Regression for the review finding on PR #251: `DRIVER_JOIN_TIMEOUT` (the
    /// outer join-or-abort budget) and `DRIVER_CANCEL_TIMEOUT` (the inner
    /// best-effort-cancel budget inside a driver's own cancellation branch) used
    /// to be the same constant, started at nearly the same instant — a coin flip
    /// over whether the driver's own `remove_flow()` landed before the outer
    /// `abort()` fired, which on the outer-wins side orphaned the registry entry
    /// forever (`sweep_expired` never reclaims a `terminal_at == None` entry).
    /// This asserts the margin exists so the two can't regress back to parity.
    #[test]
    fn driver_join_timeout_has_margin_over_cancel_timeout() {
        assert!(
            DRIVER_JOIN_TIMEOUT > DRIVER_CANCEL_TIMEOUT,
            "the outer join-or-abort budget must leave real headroom past the inner \
             best-effort-cancel budget, or the outer abort can race and win against \
             the driver's own cleanup"
        );
    }

    /// Companion to the margin test above: even if a driver is aborted before it
    /// can run its own cleanup, `join_or_abort_drivers` must reclaim the registry
    /// entry itself rather than leaving a `terminal_at == None` entry to linger
    /// forever. A real `FlowEntry` can't be constructed here (its `request` field
    /// is a `VerificationRequest`, buildable only via a live SDK `Client` — see the
    /// PR discussion), so this exercises the no-entry-present case: `abort()` must
    /// still complete without panicking when there is nothing to clean up (e.g. a
    /// driver that had already removed itself between cancellation and the join).
    #[tokio::test]
    async fn join_or_abort_drivers_abort_path_tolerates_a_missing_registry_entry() {
        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });

        join_or_abort_drivers(
            &new_registry(),
            Uuid::new_v4(),
            vec![("flow".to_owned(), handle)],
        )
        .await;
    }

    /// The dedup memory must record an id only on an explicit `mark`, never on a
    /// bare `seen` peek — otherwise a cross-user request's transient first-delivery
    /// miss would be committed as "handled" and its recovering re-delivery dropped
    /// (ADR 0040). Pure in-memory; no DB.
    #[test]
    fn handled_room_events_commit_only_on_mark() {
        let handled = HandledRoomEvents::default();

        // Peeking the same un-marked id repeatedly never commits it, so a
        // re-delivered event stays eligible to be processed.
        assert!(!handled.seen("$evt"));
        assert!(!handled.seen("$evt"));

        // After the event is fully handled and marked, a re-delivery is deduped.
        handled.mark("$evt".to_owned());
        assert!(handled.seen("$evt"));

        // Distinct ids are tracked independently.
        assert!(!handled.seen("$other"));
    }

    /// `mark_if_new` is the atomic claim the self-verification fallback relies on to
    /// mint at most one counter-request: it succeeds once, then a racing
    /// re-delivery loses the claim — and `forget` releases it so a real failure
    /// retries (ADR 0040). Pure in-memory; no DB.
    #[test]
    fn handled_room_events_claim_and_release() {
        let handled = HandledRoomEvents::default();

        // First claim wins; a second (the racing re-delivery) is rejected.
        assert!(handled.mark_if_new("$evt".to_owned()));
        assert!(!handled.mark_if_new("$evt".to_owned()));

        // Releasing the claim (mint failed) makes the id claimable again.
        handled.forget("$evt");
        assert!(handled.mark_if_new("$evt".to_owned()));
    }

    /// `TtlSet`: fresh insert vs. refresh is distinguished (the signal `add_candidate`
    /// uses to wake), removal is honored, and a zero-TTL entry is pruned on the next
    /// read (the expiry path, without sleeping).
    #[test]
    fn ttl_set_insert_refresh_remove_expire() {
        let mut set: TtlSet<String> = TtlSet::default();

        // First insert is fresh; re-inserting a live key is a refresh, not fresh.
        assert!(set.insert("a".to_owned(), Duration::from_secs(60)));
        assert!(!set.insert("a".to_owned(), Duration::from_secs(60)));
        assert!(set.contains(&"a".to_owned()));

        // Removal is immediate.
        set.remove(&"a".to_owned());
        assert!(!set.contains(&"a".to_owned()));

        // A zero-TTL entry is already expired, so the next read prunes it.
        assert!(set.insert("b".to_owned(), Duration::ZERO));
        assert!(!set.contains(&"b".to_owned()));
        assert!(set.live().is_empty());
    }

    /// A supervised restart must not strand a joined-but-not-yet-requested candidate
    /// DM: `unregister` drops only the waker, leaving live candidates for the next
    /// run to replay (ADR 0040). Rejected-invite memoization is also exercised. Pure
    /// in-memory; no DB.
    #[test]
    fn verification_rooms_unregister_keeps_candidates_and_memoizes_rejects() {
        let account_id = Uuid::new_v4();
        let room: OwnedRoomId = "!cand:localhost".try_into().unwrap();
        let rooms = VerificationRooms::new();

        // Simulate a run, a joined candidate, then a teardown (supervised restart).
        let (run_id, _rx) = rooms.register(account_id);
        rooms.add_candidate(account_id, room.clone());
        rooms.unregister(account_id, run_id);

        // The candidate survives the teardown so the new run re-subscribes it.
        assert_eq!(rooms.live_candidates(account_id), vec![room.clone()]);

        // A rejected invite is remembered (skips the per-poll membership rescan).
        let other: OwnedRoomId = "!noshare:localhost".try_into().unwrap();
        assert!(!rooms.is_recently_rejected(account_id, &other));
        rooms.mark_rejected(account_id, other.clone());
        assert!(rooms.is_recently_rejected(account_id, &other));
    }

    #[test]
    fn verification_rooms_unregister_is_run_scoped() {
        let account_id = Uuid::new_v4();
        let rooms = VerificationRooms::new();

        let (old_run, _old_rx) = rooms.register(account_id);
        let (_new_run, mut new_rx) = rooms.register(account_id);
        while new_rx.try_recv().is_ok() {}

        rooms.unregister(account_id, old_run);
        rooms.wake(account_id);

        assert!(
            new_rx.try_recv().is_ok(),
            "old run unregister must not clear the new run's waker"
        );
    }

    #[test]
    fn verification_rooms_pending_request_outlives_candidate_slot() {
        let account_id = Uuid::new_v4();
        let room: OwnedRoomId = "!pending:localhost".try_into().unwrap();
        let rooms = VerificationRooms::new();

        rooms.add_pending_request(account_id, room.clone());
        assert_eq!(rooms.live_candidates(account_id), vec![room.clone()]);

        rooms.clear_candidate(account_id, &room);
        assert!(rooms.live_candidates(account_id).is_empty());
    }

    /// Build a verification engine over the test DB. The branches exercised here
    /// all return before any homeserver/SDK contact (account-state gating and
    /// registry lookups), so the manager/data_dir are never used.
    async fn engine() -> VerificationEngine {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
        let store = Store::connect(&url, 5).await.expect("connect + migrate");
        let config = SyncConfig {
            data_dir: std::env::temp_dir().join("axon-verify-test"),
            store_key: Some("test-key".to_owned()),
            timeline_limit: 1,
            live_event_buffer: 16,
            ..SyncConfig::default()
        };
        let manager = ClientManager::new(store.clone(), config.clone());
        let (live_tx, _rx) = broadcast::channel(16);
        VerificationEngine::new(
            store,
            manager,
            new_registry(),
            live_tx,
            TaskTracker::new(),
            CancellationToken::new(),
            Arc::new(Mutex::new(HashMap::new())),
            VerificationRooms::new(),
        )
    }

    async fn delete_account(store: &Store, account_id: Uuid) {
        sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
            .bind(account_id)
            .execute(store.pool())
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn start_on_unknown_account_is_not_found() {
        let eng = engine().await;
        let err = eng
            .start(Uuid::new_v4(), None, Some("DEVICEID"))
            .await
            .unwrap_err();
        assert!(matches!(err, VerifyError::NotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn start_on_deactivated_account_is_not_active() {
        let eng = engine().await;
        let user = format!("@deact-{}:localhost", Uuid::new_v4());
        let acct = eng
            .store
            .upsert_account(&user, "https://hs.example.org")
            .await
            .unwrap();
        eng.store
            .set_account_state(acct.account_id, AccountState::Deactivated)
            .await
            .unwrap();

        let err = eng
            .start(acct.account_id, None, Some("DEVICEID"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, VerifyError::NotActive(id) if id == acct.account_id),
            "got {err:?}"
        );
        delete_account(&eng.store, acct.account_id).await;
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn start_on_deleting_account_is_being_deleted() {
        let eng = engine().await;
        let user = format!("@del-{}:localhost", Uuid::new_v4());
        let acct = eng
            .store
            .upsert_account(&user, "https://hs.example.org")
            .await
            .unwrap();
        eng.store
            .set_account_state(acct.account_id, AccountState::Deleting)
            .await
            .unwrap();

        let err = eng
            .start(acct.account_id, None, Some("DEVICEID"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, VerifyError::BeingDeleted(id) if id == acct.account_id),
            "got {err:?}"
        );
        delete_account(&eng.store, acct.account_id).await;
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn get_on_unknown_account_is_not_found() {
        let eng = engine().await;
        let err = eng.get(Uuid::new_v4(), "flow").await.unwrap_err();
        assert!(matches!(err, VerifyError::NotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn get_unknown_flow_on_known_account_is_flow_not_found() {
        let eng = engine().await;
        let user = format!("@getf-{}:localhost", Uuid::new_v4());
        let acct = eng
            .store
            .upsert_account(&user, "https://hs.example.org")
            .await
            .unwrap();

        let err = eng.get(acct.account_id, "no-such-flow").await.unwrap_err();
        assert!(matches!(err, VerifyError::FlowNotFound(_)), "got {err:?}");
        delete_account(&eng.store, acct.account_id).await;
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn list_on_unknown_account_is_not_found() {
        let eng = engine().await;
        let err = eng.list(Uuid::new_v4()).await.unwrap_err();
        assert!(matches!(err, VerifyError::NotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn list_on_known_account_with_no_flows_is_empty() {
        let eng = engine().await;
        let user = format!("@listf-{}:localhost", Uuid::new_v4());
        let acct = eng
            .store
            .upsert_account(&user, "https://hs.example.org")
            .await
            .unwrap();

        let flows = eng.list(acct.account_id).await.unwrap();
        assert!(flows.is_empty());
        delete_account(&eng.store, acct.account_id).await;
    }

    /// `confirm`/`cancel` take the lifecycle lock first, so an unknown account is a
    /// `NotFound` before any registry lookup.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn confirm_and_cancel_on_unknown_account_are_not_found() {
        let eng = engine().await;
        let id = Uuid::new_v4();
        assert!(matches!(
            eng.confirm(id, "nope").await.unwrap_err(),
            VerifyError::NotFound(_)
        ));
        assert!(matches!(
            eng.cancel(id, "nope").await.unwrap_err(),
            VerifyError::NotFound(_)
        ));
    }

    /// On a known, active account the lifecycle gate passes and an unknown flow is
    /// a `FlowNotFound` — exercising the idempotent entry points without a live SAS
    /// object.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn confirm_and_cancel_unknown_flow_on_active_account_are_flow_not_found() {
        let eng = engine().await;
        let user = format!("@cf-{}:localhost", Uuid::new_v4());
        let acct = eng
            .store
            .upsert_account(&user, "https://hs.example.org")
            .await
            .unwrap();
        assert!(matches!(
            eng.confirm(acct.account_id, "nope").await.unwrap_err(),
            VerifyError::FlowNotFound(_)
        ));
        assert!(matches!(
            eng.cancel(acct.account_id, "nope").await.unwrap_err(),
            VerifyError::FlowNotFound(_)
        ));
        delete_account(&eng.store, acct.account_id).await;
    }

    /// `confirm`/`cancel` reject a severed (deactivated) account *before* touching
    /// the flow — the lifecycle-lock fix that stops a MAC/cancel send after a
    /// logout/delete has begun tearing the session down.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn confirm_and_cancel_on_deactivated_account_are_not_active() {
        let eng = engine().await;
        let user = format!("@cfd-{}:localhost", Uuid::new_v4());
        let acct = eng
            .store
            .upsert_account(&user, "https://hs.example.org")
            .await
            .unwrap();
        eng.store
            .set_account_state(acct.account_id, AccountState::Deactivated)
            .await
            .unwrap();
        assert!(matches!(
            eng.confirm(acct.account_id, "any").await.unwrap_err(),
            VerifyError::NotActive(id) if id == acct.account_id
        ));
        assert!(matches!(
            eng.cancel(acct.account_id, "any").await.unwrap_err(),
            VerifyError::NotActive(id) if id == acct.account_id
        ));
        delete_account(&eng.store, acct.account_id).await;
    }
}
