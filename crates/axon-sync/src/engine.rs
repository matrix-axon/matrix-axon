//! The sync engine: one supervised task per account.
//!
//! [`SyncEngine::start`] spawns a task per active account (accounts come into
//! existence only via the runtime `login`/`import_token` API — there is no
//! boot-time provisioning, ADR 0024). Each task builds a
//! [`Client`](matrix_sdk::Client), starts a [`SyncService`] (Simplified
//! Sliding Sync, MSC4186), and watches its state. If the service errors or
//! terminates unexpectedly the task restarts it with exponential backoff; a
//! cancellation token drives graceful shutdown.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axon_core::{
    EphemeralFrame, InviteAddedFrame, InviteRemovedFrame, LiveEvent, LiveFrame, SenderTrustFrame,
    SyncConfig, SyncStateFrame, UnreadCountsFrame,
};
use axon_media::MediaCacheHandle;
use axon_search::IndexHandle;
use axon_store::{
    Account, AccountAuthKind, AccountDataUpsert, EventCiphertext, NewEvent, RoomInviteSnapshot,
    RoomStateUpsert, Store,
};
use matrix_sdk::deserialized_responses::EncryptionInfo;
use matrix_sdk::event_handler::{Ctx, RawEvent};
use matrix_sdk::ruma::events::room::member::MembershipState;
use matrix_sdk::ruma::events::{
    AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnySyncEphemeralRoomEvent,
    AnySyncStateEvent, AnySyncTimelineEvent,
};
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::{OwnedRoomId, RoomId, UserId};
use matrix_sdk::{Client, Room, RoomState};
use matrix_sdk_ui::room_list_service::{RoomListService, State as RoomListState};
use matrix_sdk_ui::sync_service::{State, SyncService};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::backfill::{self, BackfillHealth, BackfillParams};
use crate::error::{sdk_err, SyncError};
use crate::gateway::SdkGateway;
use crate::lifecycle::{lock_for, AccountLifecycle, IdentityLock, IdentityLocks};
use crate::manager::ClientManager;
use crate::matrix_oauth;
use crate::matrix_oauth_acquire::MatrixOAuthAcquireEngine;
use crate::redecrypt;
use crate::sync_health::SyncHealth;
use crate::verification::{
    active_flow_rooms, cancel_account_flows, new_registry, on_incoming_request,
    on_incoming_room_request, reap_expired_flows, FlowRegistry, HandledRoomEvents,
    VerificationEngine, VerificationListenerCtx, VerificationRooms,
};

/// Backoff bounds for restarting a failed per-account task.
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Outer process-shutdown budget for all tasks on the engine tracker. This is
/// deliberately not the lifecycle per-account reap path: HTTP has already drained,
/// so there are no lifecycle callers waiting on store-dir quiescence, and the
/// tracker also includes non-account tasks such as verification flow drivers.
const ENGINE_TRACKER_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Narrow a matrix-sdk/Matrix-spec `u64` timestamp or count to Postgres's
/// signed `BIGINT`, saturating rather than erroring: a value this large is
/// already meaningless as a timestamp or unread count, and the exact number
/// stops mattering long before this bound. Shared by every u64→i64 boundary
/// in this module so, e.g., a persisted unread count and its live-frame
/// broadcast (see `capture_unread_counts`, which casts this back to `u64`
/// rather than re-deriving from the unclamped source) can't diverge at the
/// boundary.
fn saturating_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// A supervised task's stop handles: the per-account cancellation token plus
/// the task's join handle. Both are needed because cancellation is cooperative —
/// `cancel` only *requests* the stop; the task then drains its sync service and
/// re-decryption queue (still holding open SQLite handles under the account's
/// store dir) before exiting. A lifecycle verb that must know the store dir is
/// quiescent (logout, ahead of a possible re-login restaging it) awaits `handle`.
pub(crate) struct AccountTask {
    pub(crate) cancel: CancellationToken,
    pub(crate) handle: tokio::task::JoinHandle<()>,
}

/// `account_id → stop handles` for the per-account supervised tasks.
///
/// Each task runs under a child of the engine-wide cancel token, registered here
/// when it is spawned. The engine-wide token still cascades to every child on
/// [`SyncEngine::shutdown`], but a single account can also be stopped on its own
/// (the logout/delete lifecycle verbs cancel just that account's token and await
/// its handle). Shared (clone of the `Arc`) between the engine and the
/// [`AccountLifecycle`] so both the boot loop and the runtime login verb register
/// onto the same map.
pub(crate) type TaskRegistry = Arc<Mutex<HashMap<Uuid, AccountTask>>>;

/// Owns the per-account sync tasks. Dropping it does not stop the tasks; call
/// [`SyncEngine::shutdown`] to cancel and join them cleanly.
pub struct SyncEngine {
    /// Tracks every supervised task — both the ones spawned at boot and the ones
    /// the lifecycle layer spawns at runtime on a successful login. `shutdown`
    /// closes and waits on it. A [`TaskTracker`] (vs. a `Vec<JoinHandle>`) is what
    /// lets tasks be added after `start` returns.
    tracker: TaskTracker,
    cancel: CancellationToken,
    /// Per-account cancellation handles, so a single account's task can be stopped
    /// without touching the others (the logout/delete verbs). Populated by
    /// [`spawn_supervised`] for both boot-time and runtime-login tasks.
    tasks: TaskRegistry,
    /// Producer end of the live-event bus. The sync tasks publish through clones
    /// of this; [`SyncEngine::live_events`] hands a clone to the API layer so
    /// each WebSocket connection can `subscribe()`.
    live_tx: broadcast::Sender<LiveFrame>,
    /// Per-account client lifecycle. Shared by the supervised sync tasks (which
    /// drive connects + retry) and the message gateway handed to the API layer
    /// (see [`SyncEngine::gateway`]).
    manager: ClientManager,
    /// Store + config handles retained so [`SyncEngine::lifecycle`] can build the
    /// runtime login port with everything a freshly logged-in account's
    /// supervised task needs.
    store: Store,
    config: SyncConfig,
    /// Per-identity lifecycle locks, owned here so every [`AccountLifecycle`] this
    /// engine hands out and every supervised task's verification watcher share the
    /// *same* lock per identity (ADR 0026).
    locks: IdentityLocks,
    /// In-memory registry of interactive SAS verification flows, shared between the
    /// [`VerificationEngine`] (the runtime port) and each account's supervised task
    /// (which listens for peer-initiated requests). Ephemeral — never persisted.
    verifications: FlowRegistry,
    /// Per-account verification room subscriptions (ADR 0040), shared between the
    /// [`VerificationEngine`] (a cross-user `start` adds its DM) and each account's
    /// sync loop (which subscribes the active set). Ephemeral, like `verifications`.
    verification_rooms: VerificationRooms,
    /// Producer handle for the search-index actor (M9), or `None` when search is
    /// disabled. Cloned into every supervised task's persist + re-decryption paths
    /// so newly ingested events are indexed, and into the lifecycle port so a
    /// deleted account's documents are purged.
    index: Option<IndexHandle>,
    /// Handle to the bounded media cache (M11), cloned into the lifecycle port so
    /// account-deletion teardown can purge a deleted account's cached media
    /// (ADR 0024 step 5).
    media: MediaCacheHandle,
    /// Disk-space health of the M10 backfill engine, shared with every account's
    /// backfill task (they write it) and the API status surface (which reads it).
    backfill_health: BackfillHealth,
    /// Per-account sync-service state, shared with every account's supervised
    /// task (they write it, on every state transition) and the API status
    /// surface (which reads it).
    sync_health: SyncHealth,
    /// Pre-account Matrix OAuth QR-login registry and SDK driver (ADR 0097).
    /// Shared by every API accessor; flows are process-ephemeral while their
    /// non-secret crash-recovery breadcrumbs live in Postgres.
    matrix_oauth_acquire: MatrixOAuthAcquireEngine,
}

impl SyncEngine {
    /// Spawn one supervised sync task per active account in the store. Returns
    /// once tasks are spawned; call [`SyncEngine::shutdown`] to stop them.
    /// Accounts come into existence only through the runtime API (`login` /
    /// `import_token`) — there is no boot-time provisioning, so a `DELETE`
    /// is durable by construction: nothing here can recreate a removed account
    /// (ADR 0024, GH #65/#66).
    pub async fn start(
        store: Store,
        config: SyncConfig,
        index: Option<IndexHandle>,
        media: MediaCacheHandle,
    ) -> Result<Self, SyncError> {
        let cancel = CancellationToken::new();
        // The bus exists for the lifetime of the engine regardless of how many
        // accounts there are (zero accounts → an idle but valid `/v1/ws`). The
        // held `_rx` is dropped immediately; `broadcast` keeps the channel open
        // as long as a `Sender` exists, so this does not close it. Capacity is
        // configurable (`sync.live_event_buffer`) — see that field's docs.
        let (live_tx, _rx) = broadcast::channel(config.live_event_buffer);
        // The client manager is the single owner of per-account clients; both the
        // supervised sync tasks and the gateway pull from it.
        let manager = ClientManager::new(store.clone(), config.clone());

        // One tracker holds every task; the runtime login verb spawns onto the
        // same tracker so a logged-in-at-runtime account shuts down with the rest.
        // The task registry is shared the same way, so the lifecycle verbs can
        // cancel a single account's task. Built here (ahead of the spawn loop) so
        // the boot reconcile can drive the runtime delete verb over the same
        // machinery.
        let tracker = TaskTracker::new();
        let tasks: TaskRegistry = Arc::new(Mutex::new(HashMap::new()));
        // Owned here (not inside `AccountLifecycle`) so the reconcile-time port, the
        // API-time port, and the supervised watchers all share one lock per identity.
        let locks: IdentityLocks = Arc::new(Mutex::new(HashMap::new()));
        // Shared by the verification port and every account's incoming-request
        // listener. Ephemeral — lives only as long as the engine.
        let verifications = new_registry();
        // Shared the same way: cross-user verification room subscriptions (ADR 0040).
        let verification_rooms = VerificationRooms::new();
        // Shared disk-space health for the backfill engine (M10): one handle for
        // the whole engine, cloned into each account's backfill task and exposed to
        // the API status surface. Carries the guarded filesystem so `/v1/status`
        // reads free space live.
        let backfill_health = BackfillHealth::new(Some(backfill::guard_path(&config)));
        // Shared per-account sync-service state (see module docs): one handle for
        // the whole engine, written by each account's supervised task and read by
        // the API status surface.
        let sync_health = SyncHealth::new();

        // Crash recovery (ADR 0024), before any account is brought online and
        // before the HTTP listener binds (`axon-server` serves only after `start`
        // returns), so neither sweep races API traffic or a supervised task
        // creating a fresh store dir: finish any interrupted account deletion, then
        // prune row-less store dirs.
        let lifecycle = AccountLifecycle::new(
            store.clone(),
            config.clone(),
            manager.clone(),
            live_tx.clone(),
            cancel.clone(),
            tracker.clone(),
            tasks.clone(),
            locks.clone(),
            verifications.clone(),
            verification_rooms.clone(),
            index.clone(),
            media.clone(),
            backfill_health.clone(),
            sync_health.clone(),
        );
        crate::reconcile::reconcile_matrix_oauth_acquires(&config, &store).await?;
        crate::reconcile::reconcile_deleting(&lifecycle, &store).await;
        crate::reconcile::prune_orphan_store_dirs(&config, &store).await;
        crate::reconcile::prune_orphan_media_dirs(&media, &store).await;
        let matrix_oauth_acquire = MatrixOAuthAcquireEngine::new(
            store.clone(),
            config.clone(),
            lifecycle.clone(),
            tracker.clone(),
            cancel.clone(),
        );

        // `list_accounts` returns only `active` rows, so a `deactivated` or
        // `deleting` account never gets a supervised task (ADR 0022). Listed after
        // the reconcile so a just-completed deletion is already gone.
        let accounts = store.list_accounts().await?;
        if accounts.is_empty() {
            tracing::warn!("no active accounts; sync engine idle");
        }

        for account in accounts {
            spawn_supervised(
                &tracker,
                &tasks,
                store.clone(),
                config.clone(),
                account,
                cancel.clone(),
                live_tx.clone(),
                manager.clone(),
                locks.clone(),
                verifications.clone(),
                verification_rooms.clone(),
                index.clone(),
                backfill_health.clone(),
                sync_health.clone(),
            );
        }

        // Background reaper for terminal verification flows, so their grace TTL is
        // honored even on an account with no further verify API traffic to drive
        // the lazy sweep. Runs on the same tracker under a child of the engine
        // token, so `shutdown` cancels and joins it with everything else.
        tracker.spawn(reap_expired_flows(
            verifications.clone(),
            cancel.child_token(),
        ));
        tracker.spawn(matrix_oauth_acquire.clone().reap_loop(cancel.child_token()));

        Ok(SyncEngine {
            tracker,
            cancel,
            tasks,
            live_tx,
            manager,
            store,
            config,
            locks,
            verifications,
            verification_rooms,
            index,
            media,
            backfill_health,
            sync_health,
            matrix_oauth_acquire,
        })
    }

    /// A message gateway over the per-account clients, for the API layer's send
    /// path. `axon-server` wraps this in an adapter implementing its
    /// `MessageSender` port; the returned value is cheap to construct and clone.
    pub fn gateway(&self) -> SdkGateway {
        SdkGateway::new(self.manager.clone(), self.store.clone())
    }

    /// An authenticated media fetcher over the per-account clients, for the API
    /// layer's `GET /v1/media/{account_id}/…` route. `axon-server` composes this
    /// behind the `axon-media` disk cache and adapts the pair onto its
    /// `MediaProxy` port. `fetch_timeout` bounds each upstream download.
    pub fn media_fetcher(&self, fetch_timeout: std::time::Duration) -> crate::media::SdkMediaProxy {
        crate::media::SdkMediaProxy::new(self.manager.clone(), fetch_timeout)
    }

    /// A producer handle for the live-event bus. The API layer holds this in its
    /// router state and calls [`broadcast::Sender::subscribe`] once per
    /// `/v1/ws` connection. Cloning is cheap and does not affect delivery.
    pub fn live_events(&self) -> broadcast::Sender<LiveFrame> {
        self.live_tx.clone()
    }

    /// The runtime account-lifecycle port (login, …), for the API layer's
    /// lifecycle routes. `axon-server` wraps this in an adapter implementing its
    /// `AccountLifecycle` port; the returned value shares this engine's task
    /// tracker, so an account logged in at runtime is supervised and shut down
    /// alongside the boot-time ones.
    pub fn lifecycle(&self) -> AccountLifecycle {
        AccountLifecycle::new(
            self.store.clone(),
            self.config.clone(),
            self.manager.clone(),
            self.live_tx.clone(),
            self.cancel.clone(),
            self.tracker.clone(),
            self.tasks.clone(),
            self.locks.clone(),
            self.verifications.clone(),
            self.verification_rooms.clone(),
            self.index.clone(),
            self.media.clone(),
            self.backfill_health.clone(),
            self.sync_health.clone(),
        )
    }

    /// The pre-account Matrix OAuth QR-login flow engine for `/v1/` acquire
    /// routes. Cloning preserves the single shared registry and global limit.
    pub fn matrix_oauth_acquire(&self) -> MatrixOAuthAcquireEngine {
        self.matrix_oauth_acquire.clone()
    }

    /// The backfill engine's disk-space health, for the API status surface
    /// (`GET /v1/status`). Cheap to clone (an `Arc` internally).
    pub fn backfill_health(&self) -> BackfillHealth {
        self.backfill_health.clone()
    }

    /// Per-account sync-service state, for the API status surface
    /// (`GET /v1/status`). Cheap to clone (an `Arc` internally).
    pub fn sync_health(&self) -> SyncHealth {
        self.sync_health.clone()
    }

    /// The runtime device-verification port, for the API layer's verify routes.
    /// `axon-server` wraps this in an adapter implementing its `VerificationService`
    /// port. Shares this engine's flow registry, task tracker, and cancel token, so
    /// verification driver tasks are supervised and shut down with the engine, and
    /// the flows it tracks are the same ones each account's incoming-request
    /// listener registers.
    pub fn verification(&self) -> VerificationEngine {
        VerificationEngine::new(
            self.store.clone(),
            self.manager.clone(),
            self.verifications.clone(),
            self.live_tx.clone(),
            self.tracker.clone(),
            self.cancel.clone(),
            self.locks.clone(),
            self.verification_rooms.clone(),
        )
    }

    /// The runtime sender-trust port, for the API layer's verification-bundle
    /// route (M7c). `axon-server` wraps this in an adapter implementing its
    /// `SenderTrustService` port. Shares this engine's store and client manager;
    /// it's a read-only port and takes no lifecycle lock (see [`trust`]).
    pub fn sender_trust(&self) -> crate::trust::SenderTrustEngine {
        crate::trust::SenderTrustEngine::new(self.store.clone(), self.manager.clone())
    }

    /// The runtime device-list port, for the API layer's device-picker route
    /// (M16, ADR 0060). `axon-server` wraps this in an adapter implementing
    /// its `DeviceListService` port. Shares this engine's store and client
    /// manager; read-only, no lifecycle lock (see [`devices`]).
    pub fn devices(&self) -> crate::devices::DeviceListEngine {
        crate::devices::DeviceListEngine::new(self.store.clone(), self.manager.clone())
    }

    /// The runtime cached member-profile port, for best-effort avatar
    /// enrichment on the API's `/members` route. Shares this engine's store and
    /// client manager; read-only, no lifecycle lock (see [`member_profiles`]).
    ///
    /// [`member_profiles`]: crate::member_profiles
    pub fn member_profiles(&self) -> crate::member_profiles::MemberProfileEngine {
        crate::member_profiles::MemberProfileEngine::new(self.store.clone(), self.manager.clone())
    }

    /// Cancel all tracked sync tasks and wait for them to finish. Safe to call
    /// without canceling the token first — this method cancels it internally.
    /// Returns whether every tracked task drained within the shutdown budget.
    pub async fn shutdown(self) -> bool {
        self.cancel.cancel();
        // Close the tracker so `wait` can complete; no new tasks are spawned after
        // shutdown begins (the lifecycle port would spawn onto a closed tracker,
        // which is a no-op-and-error it already guards against).
        self.tracker.close();
        if tokio::time::timeout(ENGINE_TRACKER_DRAIN_TIMEOUT, self.tracker.wait())
            .await
            .is_ok()
        {
            return true;
        }

        tracing::error!(
            timeout_secs = ENGINE_TRACKER_DRAIN_TIMEOUT.as_secs(),
            "sync engine tasks did not finish draining within the timeout; continuing process shutdown"
        );
        false
    }
}

/// Spawn a supervised sync task for `account` onto `tracker`. Shared by boot
/// (one call per active account) and the runtime login verb (one call when a
/// login succeeds), so both paths get identical supervision + shutdown.
///
/// The task runs under a fresh child of the engine-wide `cancel` token; its
/// token + join handle are registered in `tasks` under the account id so a
/// lifecycle verb can stop just this account *and await its drain*. A stale
/// entry for the id should not exist (logout awaits termination before the
/// identity can log back in), but if one does it is cancelled and replaced as a
/// backstop, so an account can never end up with two registered tasks.
// The supervised task genuinely needs every one of these handles; bundling them
// into a context struct would only move the same fields behind one more name.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_supervised(
    tracker: &TaskTracker,
    tasks: &TaskRegistry,
    store: Store,
    config: SyncConfig,
    account: Account,
    cancel: CancellationToken,
    live_tx: broadcast::Sender<LiveFrame>,
    manager: ClientManager,
    locks: IdentityLocks,
    verifications: FlowRegistry,
    verification_rooms: VerificationRooms,
    index: Option<IndexHandle>,
    backfill_health: BackfillHealth,
    sync_health: SyncHealth,
) {
    let task_cancel = cancel.child_token();
    let account_id = account.account_id;
    let handle = tracker.spawn(supervise_account(
        store,
        config,
        account,
        task_cancel.clone(),
        live_tx,
        manager,
        locks,
        tracker.clone(),
        verifications,
        verification_rooms,
        index,
        backfill_health,
        sync_health,
    ));
    if let Some(stale) = tasks.lock().expect("task registry poisoned").insert(
        account_id,
        AccountTask {
            cancel: task_cancel,
            handle,
        },
    ) {
        stale.cancel.cancel();
    }
}

/// Supervise a single account: run it, and on failure restart with exponential
/// backoff until the cancellation token fires.
#[allow(clippy::too_many_arguments)]
async fn supervise_account(
    store: Store,
    config: SyncConfig,
    account: Account,
    cancel: CancellationToken,
    live_tx: broadcast::Sender<LiveFrame>,
    manager: ClientManager,
    locks: IdentityLocks,
    tracker: TaskTracker,
    verifications: FlowRegistry,
    verification_rooms: VerificationRooms,
    index: Option<IndexHandle>,
    backfill_health: BackfillHealth,
    sync_health: SyncHealth,
) {
    let mut backoff = BACKOFF_START;

    loop {
        if cancel.is_cancelled() {
            return;
        }

        match run_account(
            &store,
            &config,
            &account,
            &cancel,
            &live_tx,
            &manager,
            &locks,
            &tracker,
            &verifications,
            &verification_rooms,
            index.as_ref(),
            &backfill_health,
            &sync_health,
        )
        .await
        {
            Ok(()) => {
                // Clean stop (cancellation requested).
                return;
            }
            Err(err) => {
                // Drop the cached client so the next attempt reconnects cleanly
                // (a stale session/connection won't be reused across a restart).
                // This awaits the slot lock (see `evict`'s doc), so it can block
                // behind a concurrent `get_or_connect` on this account for as
                // long as that connect takes (unbounded — no connect timeout).
                // Race it against cancellation so a shutdown/logout isn't stalled
                // by a hung homeserver: on cancel, `sever_session` still tears the
                // client down correctly afterward via `take` (which runs only
                // once this task has exited, per `reap_task`), so dropping this
                // eviction attempt loses nothing there; on full process shutdown
                // it doesn't matter either. It only matters for an ordinary
                // restart-after-failure, where `cancel` isn't firing and this
                // always resolves via the eviction itself.
                tokio::select! {
                    () = manager.evict(account.account_id) => {}
                    () = cancel.cancelled() => return,
                }
                if let Some(sync_state) = sync_health.set_error(account.account_id) {
                    let _ = live_tx.send(
                        SyncStateFrame {
                            account_id: account.account_id,
                            sync_state,
                        }
                        .into(),
                    );
                }
                tracing::error!(
                    account_id = %account.account_id,
                    error = %err,
                    backoff_secs = backoff.as_secs(),
                    "account sync task failed; restarting after backoff"
                );
            }
        }

        // Wait out the backoff, but wake immediately on cancellation.
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Shared context injected into the per-account event handler. Also handed to the
/// backfill task (M10), which persists paged events through the same path.
#[derive(Clone)]
pub(crate) struct PersistContext {
    pub(crate) store: Store,
    pub(crate) account_id: Uuid,
    /// Producer end of the live-event bus; [`persist_timeline_event`] publishes
    /// each freshly persisted event to it for `/v1/ws` fan-out.
    pub(crate) live_tx: broadcast::Sender<LiveFrame>,
    /// Search-index producer (M9), or `None` when search is disabled. Each
    /// persisted event is enqueued for (re)indexing from the resolved projection.
    pub(crate) index: Option<IndexHandle>,
    /// This account's own Matrix user id, so the room-state handler can recognize
    /// a membership event that is *this user* leaving/being banned (M10 purge).
    pub(crate) local_user_id: Arc<str>,
    /// When set, a leave/ban of the local user destructively purges the room's
    /// stored events + search documents (ADR 0044). Off by default.
    pub(crate) purge_on_leave: bool,
}

/// Parse an event's raw JSON text into a [`serde_json::Value`], logging (with
/// `what` naming the caller's context, e.g. `"state event"`) and returning
/// `None` on failure instead of propagating the parse error. Shared by every
/// handler below that needs generic field access (`type`/`content`/…) a typed
/// `Ev` doesn't expose uniformly — extracted so a fix to this parse/log/skip
/// shape (e.g. a size cap) only needs to land once.
fn parse_raw_json(json: &str, account_id: Uuid, what: &'static str) -> Option<serde_json::Value> {
    match serde_json::from_str(json) {
        Ok(v) => Some(v),
        Err(err) => {
            tracing::warn!(account_id = %account_id, what, error = %err, "failed to parse raw event JSON; skipping");
            None
        }
    }
}

/// Event handler: persist every synced timeline event to Postgres.
///
/// For E2EE rooms, matrix-rust-sdk decrypts the megolm payload before
/// dispatching, so `ev` and `raw` already carry the plaintext content and
/// `enc_info` describes how it was decrypted. UTDs arrive as `m.room.encrypted`
/// events with the ciphertext as content and `enc_info = None`; the
/// re-decryption queue back-fills their `content` once keys arrive.
///
/// Alongside the `events` row this writes the crypto sibling rows (ADR 0015): the
/// ciphertext sibling for UTDs (the only events whose ciphertext the SDK hands
/// us), and the crypto-provenance siblings from `enc_info` for decrypted events.
async fn persist_timeline_event(
    ev: AnySyncTimelineEvent,
    room: Room,
    raw: RawEvent,
    enc_info: Option<EncryptionInfo>,
    Ctx(ctx): Ctx<PersistContext>,
) {
    let Some(raw_val) = parse_raw_json(raw.get(), ctx.account_id, "timeline event") else {
        return;
    };
    // The live sync path: persist and emit the fresh event to `/v1/ws`.
    persist_event_core(
        &ctx,
        &ev,
        raw_val,
        room.room_id().as_str(),
        enc_info.as_ref(),
        true,
    )
    .await;
}

/// Persist one event fetched by history backfill (M10). Back-pagination
/// (`/messages`) does not dispatch through `add_event_handler`, so the backfill
/// driver calls this to run each paged event through the same ingestion path as
/// live sync — minus the live `/v1/ws` emit (see [`persist_event_core`]).
/// A paged event that the SDK could not decrypt (keys not yet imported) arrives
/// as a UTD, exactly as on the live path; the re-decryption queue back-fills it
/// once keys arrive.
pub(crate) async fn persist_backfilled_event(
    ctx: &PersistContext,
    room: &Room,
    tev: &matrix_sdk::deserialized_responses::TimelineEvent,
) {
    let raw = tev.raw();
    let ev: AnySyncTimelineEvent = match raw.deserialize() {
        Ok(ev) => ev,
        Err(err) => {
            tracing::warn!(
                account_id = %ctx.account_id,
                error = %err,
                "failed to deserialize backfilled event; skipping"
            );
            return;
        }
    };
    let Some(raw_val) = parse_raw_json(raw.json().get(), ctx.account_id, "backfilled event") else {
        return;
    };
    persist_event_core(
        ctx,
        &ev,
        raw_val,
        room.room_id().as_str(),
        tev.encryption_info().map(|info| info.as_ref()),
        false,
    )
    .await;
}

/// Persist one timeline event — live (`emit_live = true`) or backfilled
/// (`emit_live = false`) — through the shared ingestion path: the hot columns,
/// `upsert_event` (which transactionally appends the M8/M9 search-outbox
/// obligation), and the crypto sibling rows. Only the live path emits a `/v1/ws`
/// frame: replaying deep history through the live bus would flood subscribers
/// with old events mislabeled as just-arrived, and backfilled history reaches
/// clients through timeline reads (M8) and search (M9) instead — both driven by
/// `upsert_event`, not the emit.
async fn persist_event_core(
    ctx: &PersistContext,
    ev: &AnySyncTimelineEvent,
    raw_val: serde_json::Value,
    room_id: &str,
    enc_info: Option<&EncryptionInfo>,
    emit_live: bool,
) {
    // Extract event_type as an owned String so raw_val can be moved into NewEvent below.
    let event_type: String = raw_val
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_owned();
    let is_utd = event_type == "m.room.encrypted";
    // An event that is still `m.room.encrypted` at dispatch is one the SDK could
    // not decrypt (a UTD): its `content` is the megolm ciphertext envelope, not
    // plaintext. Persist `content = NULL` so the column means "decrypted payload"
    // — `content IS NOT NULL` is then a true decrypted signal, and the
    // re-decryption queue can find pending UTDs by `content IS NULL`. The full
    // ciphertext (incl. `session_id`) is preserved in `raw_event` for re-decryption.
    // Once the SDK decrypts a megolm event it dispatches it with the cleartext
    // type, so this branch is skipped and the real plaintext content is stored.
    let content = if is_utd {
        None
    } else {
        raw_val.get("content").cloned()
    };
    // For a UTD, lift the megolm `session_id` into its own column so the
    // re-decryption queue can match arriving room keys to this row without
    // re-parsing the envelope. Owned (not borrowed from `raw_val`) so `raw_val`
    // can still move into `raw_event` below.
    let megolm_session_id: Option<String> = if is_utd {
        crate::redecrypt::megolm_session_id(&raw_val).map(str::to_owned)
    } else {
        None
    };
    // Hot columns. `redacts` applies to redaction events (never encrypted);
    // `relates_to` / `decrypted_body_text` come from the plaintext content, so
    // they're only available once decrypted (a re-decrypted UTD picks them up via
    // the re-decryption back-fill, not here). Owned so raw_val can still move.
    let redacts: Option<String> = crate::meta::redacts(&raw_val).map(str::to_owned);
    let relates_to = content.as_ref().and_then(crate::meta::relates_to);
    let decrypted_body_text: Option<String> = content
        .as_ref()
        .and_then(|c| crate::meta::body_text(c).map(str::to_owned));
    // Capture the ciphertext envelope before raw_val is moved — only UTDs carry it.
    let ciphertext = if is_utd {
        raw_val.get("content").cloned()
    } else {
        None
    };
    let origin_ts = saturating_i64(u64::from(ev.origin_server_ts().0));
    let event_id = ev.event_id().as_str().to_owned();
    let room_id = room_id.to_owned();
    let state_key = raw_val
        .get("state_key")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let prev_content = crate::meta::prev_content(&raw_val);

    let new_ev = NewEvent {
        event_id: &event_id,
        room_id: &room_id,
        account_id: ctx.account_id,
        sender: ev.sender().as_str(),
        origin_ts,
        event_type: &event_type,
        content,
        raw_event: raw_val,
        megolm_session_id: megolm_session_id.as_deref(),
        redacts: redacts.as_deref(),
        relates_to,
        decrypted_body_text: decrypted_body_text.as_deref(),
    };

    // The returned id is the event's arrival order, which the live frame carries
    // so a `/v1/ws` subscriber and a later timeline read agree on it (ADR 0089).
    let arrival_order = match ctx.store.upsert_event(&new_ev).await {
        Ok(arrival_order) => arrival_order,
        Err(err) => {
            // Don't write sibling rows if the event row didn't land — they FK to it.
            tracing::warn!(
                account_id = %ctx.account_id,
                event_id = %event_id,
                error = %err,
                "failed to persist event"
            );
            return;
        }
    };
    tracing::debug!(
        account_id = %ctx.account_id,
        event_id = %event_id,
        room_id = %room_id,
        event_type = event_type.as_str(),
        arrival_order,
        "persisted event"
    );

    // Poke the search indexer (M9). The durable indexing obligation — this
    // event's own document plus any relation/redaction *target* whose document it
    // changes — was already written to `search_outbox` transactionally by
    // `upsert_event`, so this is only a best-effort wakeup hint: a dropped notify
    // costs nothing because the next drain (a later notify, the periodic tick, or a
    // restart) still applies the on-disk obligation.
    if let Some(index) = ctx.index.as_ref() {
        index.notify();
    }

    // Sibling rows are best-effort: a failure here must not take down sync. Done
    // *before* the live emit so the frame can carry the verdict actually persisted
    // (the `COALESCE`d snapshot), not the freshly-derived one — they diverge on a
    // duplicate delivery whose trust changed, and a live subscriber must see the
    // same immutable snapshot a later timeline read returns (ADR 0031).
    let stored_trust = persist_event_siblings(ctx, &event_id, &room_id, ciphertext, enc_info).await;

    // Fan the event out to any live `/v1/ws` subscribers. Skip the work entirely
    // when nobody is listening (the common case for a headless server) so we
    // don't clone the content needlessly. `send` errors only when there are no
    // receivers — harmless to ignore (a receiver may have dropped between the
    // count check and the send), and never fatal to sync.
    if emit_live && ctx.live_tx.receiver_count() > 0 {
        // The effective stored verdict, so a live subscriber and a subsequent
        // timeline read agree. `None` for UTDs (no `enc_info` yet) and unencrypted
        // events.
        let _ = ctx.live_tx.send(LiveFrame::Timeline(LiveEvent {
            account_id: ctx.account_id,
            event_id: event_id.clone(),
            room_id: room_id.clone(),
            sender: ev.sender().as_str().to_owned(),
            state_key: state_key.clone(),
            prev_content,
            arrival_order,
            origin_ts,
            event_type: event_type.clone(),
            content: new_ev.content.clone(),
            body: decrypted_body_text.clone(),
            relates_to: new_ev.relates_to.clone(),
            sender_trust: stored_trust,
        }));
    }
}

/// Write the crypto sibling rows for an event already persisted to `events`.
/// `ciphertext` is the `m.room.encrypted` content for UTDs (`None` otherwise);
/// `enc_info` is the SDK decryption info for decrypted events (`None` for UTDs).
///
/// Returns the **effective stored `sender_trust`** verdict (the `COALESCE`d value
/// the crypto-sibling upsert settled on), so the caller can emit a live frame
/// carrying the persisted snapshot rather than the freshly-derived verdict — they
/// diverge on a duplicate delivery whose trust changed (ADR 0031). `None` for a
/// UTD (no `enc_info`), an unencrypted event, or if the sibling write failed.
async fn persist_event_siblings(
    ctx: &PersistContext,
    event_id: &str,
    room_id: &str,
    ciphertext: Option<serde_json::Value>,
    enc_info: Option<&EncryptionInfo>,
) -> Option<String> {
    if let Some(ciphertext) = ciphertext {
        let algorithm = ciphertext
            .get("algorithm")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let sender_key = ciphertext
            .get("sender_key")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let session_id = ciphertext
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let row = EventCiphertext {
            account_id: ctx.account_id,
            event_id,
            room_id,
            algorithm: &algorithm,
            sender_key: sender_key.as_deref(),
            session_id: session_id.as_deref(),
            ciphertext,
        };
        if let Err(err) = ctx.store.insert_event_ciphertext(&row).await {
            tracing::warn!(account_id = %ctx.account_id, event_id, error = %err, "failed to persist ciphertext sibling");
        }
    }

    if let Some(info) = enc_info {
        let meta = crate::meta::crypto_meta(info);
        match ctx
            .store
            .upsert_event_crypto(&meta.as_event_crypto(ctx.account_id, event_id))
            .await
        {
            Ok(stored_trust) => return stored_trust,
            Err(err) => {
                tracing::warn!(account_id = %ctx.account_id, event_id, error = %err, "failed to persist crypto sibling");
            }
        }
    }
    None
}

/// Event handler: project a room-state event into the `room_state` table (the
/// derived current-value view, maintained by upsert). The raw state event is
/// also persisted to `events` by [`persist_timeline_event`]; this writes the
/// resolved tuple a room-summary read needs. Identity fields come from the typed
/// event; `type`/`state_key`/`content` from the raw JSON so the exact content
/// (incl. unknown fields) is preserved.
async fn persist_room_state_event(
    ev: AnySyncStateEvent,
    room: Room,
    raw: RawEvent,
    Ctx(ctx): Ctx<PersistContext>,
) {
    let Some(raw_val) = parse_raw_json(raw.get(), ctx.account_id, "state event") else {
        return;
    };
    let event_type = raw_val
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    // Singleton state (m.room.name, m.room.topic) carries state_key "".
    let state_key = raw_val
        .get("state_key")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();
    let content = raw_val.get("content").cloned();
    let event_id = ev.event_id().as_str().to_owned();
    let sender = ev.sender().as_str().to_owned();
    let origin_ts = saturating_i64(u64::from(ev.origin_server_ts().0));
    let room_id = room.room_id().as_str().to_owned();

    let upsert = RoomStateUpsert {
        account_id: ctx.account_id,
        room_id: &room_id,
        event_type: &event_type,
        state_key: &state_key,
        event_id: &event_id,
        sender: &sender,
        origin_ts,
        content,
    };
    if let Err(err) = ctx
        .store
        .upsert_room_state_for_local_user(&upsert, Some(ctx.local_user_id.as_ref()))
        .await
    {
        tracing::warn!(account_id = %ctx.account_id, room_id = %room_id, event_type = event_type.as_str(), error = %err, "failed to persist room state");
    } else {
        tracing::debug!(account_id = %ctx.account_id, room_id = %room_id, event_type = event_type.as_str(), state_key = state_key.as_str(), "persisted room state");
    }

    // M10 purge-on-leave (ADR 0044): when this state event is *this account*
    // leaving or being banned and the operator enabled destructive purge, remove
    // the room's stored events + search documents. Idempotent — a later re-join
    // re-backfills. Off by default; when off, left rooms are retained and merely
    // hidden from search by the membership filter.
    if ctx.purge_on_leave
        && event_type == "m.room.member"
        && state_key.as_str() == &*ctx.local_user_id
    {
        let membership = raw_val
            .get("content")
            .and_then(|c| c.get("membership"))
            .and_then(serde_json::Value::as_str);
        if matches!(membership, Some("leave") | Some("ban")) {
            match ctx.store.purge_room(ctx.account_id, &room_id).await {
                Ok(()) => {
                    // Wake the indexer so it applies the room-purge obligation
                    // `purge_room` just enqueued.
                    if let Some(index) = ctx.index.as_ref() {
                        index.notify();
                    }
                    tracing::info!(account_id = %ctx.account_id, room_id = %room_id, "purged room on leave");
                }
                Err(err) => {
                    tracing::warn!(account_id = %ctx.account_id, room_id = %room_id, error = %err, "failed to purge room on leave");
                }
            }
        }
    }
}

/// Event handler: per-room account data (fully-read markers, tags, …) → the
/// `account_data` table, scoped to the room.
async fn persist_room_account_data(
    _ev: AnyRoomAccountDataEvent,
    room: Room,
    raw: RawEvent,
    Ctx(ctx): Ctx<PersistContext>,
) {
    let room_id = room.room_id().as_str().to_owned();
    persist_account_data(&ctx, Some(&room_id), &raw).await;
}

/// Event handler: global (account-wide) account data (push rules, m.direct,
/// ignored users, …) → the `account_data` table, global scope. No `Room` arg —
/// global account data has no room.
async fn persist_global_account_data(
    _ev: AnyGlobalAccountDataEvent,
    raw: RawEvent,
    Ctx(ctx): Ctx<PersistContext>,
) {
    persist_account_data(&ctx, None, &raw).await;
}

/// Shared account-data upsert for both scopes. `room_id = None` is global.
/// Account-data events carry only `type` + `content` (no event_id/sender/ts),
/// both read from the raw JSON; `content` is required (the column is NOT NULL).
async fn persist_account_data(ctx: &PersistContext, room_id: Option<&str>, raw: &RawEvent) {
    let Some(raw_val) = parse_raw_json(raw.get(), ctx.account_id, "account data") else {
        return;
    };
    let event_type = raw_val
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let Some(content) = raw_val.get("content").cloned() else {
        tracing::warn!(account_id = %ctx.account_id, event_type = event_type.as_str(), "account data event has no content; skipping");
        return;
    };

    let upsert = AccountDataUpsert {
        account_id: ctx.account_id,
        room_id,
        event_type: &event_type,
        content,
    };
    if let Err(err) = ctx.store.upsert_account_data(&upsert).await {
        tracing::warn!(account_id = %ctx.account_id, room_id = ?room_id, event_type = event_type.as_str(), error = %err, "failed to persist account data");
    } else {
        tracing::debug!(account_id = %ctx.account_id, room_id = ?room_id, event_type = event_type.as_str(), "persisted account data");
    }
}

/// Context for the generic ephemeral-event passthrough (ADR 0056). Deliberately
/// separate from [`PersistContext`]: nothing here is persisted, so there is no
/// `store`/`index` to carry.
#[derive(Clone)]
struct EphemeralCtx {
    account_id: Uuid,
    /// Producer end of the live-event bus; [`forward_ephemeral_event`] publishes
    /// each allowlisted event to it for `/v1/ws` fan-out.
    live_tx: broadcast::Sender<LiveFrame>,
    /// Event types forwarded verbatim (`config.ephemeral_event_types`). An
    /// event type not in this set is dropped — fails closed.
    allowlist: Arc<HashSet<String>>,
}

/// A raw ephemeral event larger than this (JSON byte length, checked before
/// any parsing) is dropped rather than forwarded. `live_tx` is one broadcast
/// bus shared by every account this process syncs, so an oversized `m.typing`/
/// `m.receipt` EDU from one account's homeserver would otherwise cost every
/// other account's `/v1/ws` clients ring-buffer space and lag risk. Mirrors
/// the `RELATION_READ_CAP`/`REACTION_AGG_PAIR_CAP` bounds `axon-store` places
/// on similarly homeserver-influenced data.
const EPHEMERAL_EVENT_MAX_BYTES: usize = 262_144;

/// Build an [`EphemeralFrame`] from a raw ephemeral-room-event JSON body, or
/// `None` if the event's `type` is not on `allowlist` or `content` is missing.
/// A pure function (unlike the persist_* handlers, which inline their
/// parsing) so the allowlist behavior gets a fast, SDK-free unit test. Takes
/// `raw` by value so `content` can be moved out of it instead of cloned.
fn ephemeral_frame_from_raw(
    account_id: Uuid,
    room_id: Option<&str>,
    mut raw: serde_json::Value,
    allowlist: &HashSet<String>,
) -> Option<EphemeralFrame> {
    let event_type = raw
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    if !allowlist.contains(event_type.as_str()) {
        tracing::debug!(
            account_id = %account_id,
            event_type = event_type.as_str(),
            "ephemeral event type not on allowlist; skipping"
        );
        return None;
    }
    let Some(content) = raw.get_mut("content").map(serde_json::Value::take) else {
        tracing::warn!(
            account_id = %account_id,
            event_type = event_type.as_str(),
            "ephemeral event has no content; skipping"
        );
        return None;
    };
    Some(EphemeralFrame {
        account_id,
        room_id: room_id.map(str::to_owned),
        event_type,
        content,
    })
}

/// Event handler: forward an allowlisted ephemeral room event (`m.typing`,
/// `m.receipt`, …) verbatim onto the live-event bus (ADR 0056). Axon never
/// persists these — they are transient overlays with no store row — so unlike
/// the persist_* handlers above, this only touches the bus.
///
/// Takes `Raw<AnySyncEphemeralRoomEvent>` rather than the bare enum: it still
/// drives the same `HandlerKind::EphemeralRoomData` dispatch/routing, but the
/// SDK only has to capture the raw JSON bytes to build it, not fully
/// deserialize into the enum's variant tree — the handler discards the typed
/// event and re-parses the raw JSON itself below, so a second full parse
/// would otherwise be pure waste on every ephemeral event, in every room, in
/// every account.
async fn forward_ephemeral_event(
    _ev: Raw<AnySyncEphemeralRoomEvent>,
    room: Room,
    raw: RawEvent,
    Ctx(ctx): Ctx<EphemeralCtx>,
) {
    if ctx.live_tx.receiver_count() == 0 {
        return;
    }
    let json = raw.get();
    if json.len() > EPHEMERAL_EVENT_MAX_BYTES {
        tracing::warn!(
            account_id = %ctx.account_id,
            len = json.len(),
            cap = EPHEMERAL_EVENT_MAX_BYTES,
            "ephemeral event exceeds size cap; dropping"
        );
        return;
    }
    let Some(raw_val) = parse_raw_json(json, ctx.account_id, "ephemeral event") else {
        return;
    };
    let room_id = room.room_id().as_str();
    let Some(frame) =
        ephemeral_frame_from_raw(ctx.account_id, Some(room_id), raw_val, &ctx.allowlist)
    else {
        return;
    };
    let _ = ctx.live_tx.send(LiveFrame::Ephemeral(frame));
}

/// How often the sync loop polls for new verification candidate invites and
/// re-derives the explicit subscription set (ADR 0040).
const VERIFICATION_POLL: Duration = Duration::from_secs(5);

/// How often [`watch_unread_counts`] re-walks every joined room as a
/// self-healing backstop (issue #313, ADR 0070). `room_info_notable_update_receiver`
/// is a lossy broadcast channel with no dedicated "notification count changed"
/// reason bit, so a `Lagged` gap could otherwise leave a room's cached count
/// stale until something else about that room changes.
const UNREAD_COUNTS_RESWEEP: Duration = Duration::from_secs(300);

/// Bound on concurrent `capture_unread_counts` calls within one
/// [`sweep_unread_counts`] pass, so an account with many joined rooms doesn't
/// fan out one Postgres round trip per room unbounded on every sweep.
const UNREAD_COUNTS_SWEEP_CONCURRENCY: usize = 8;

/// Bound on one upstream room probe in [`reconcile_upstream_rooms`] (ADR 0090).
/// A rejection from a purged room has been observed taking nearly 4s, so this is
/// generous — but it is a background sweep, and nothing waits on it.
const UPSTREAM_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// How many suspect rooms one reconcile pass probes. Each probe is an upstream
/// round trip, and suspicion accrues only from a *failed* room-scoped call, so
/// the queue is normally empty; a backlog drains a few rooms per
/// [`UNREAD_COUNTS_RESWEEP`] rather than bursting.
const UPSTREAM_PROBES_PER_SWEEP: usize = 8;

/// Why a room must not carry an unread count at all, or `None` when it may.
///
/// Both cases are rooms the account is still nominally joined to, whose counts
/// can never again be corrected by reading them:
///
/// - **Absent upstream** (ADR 0090): the homeserver no longer serves the room, so
///   no read receipt can land and matrix-sdk will never recompute. Confirmed by
///   [`reconcile_upstream_rooms`]'s probe, not guessed from one failure.
/// - **Tombstoned**: the room has been replaced, and `Store::list_rooms` already
///   hides it — so a count here is invisible to clients but still sums into
///   totals and sits in the table forever. Its successor carries the
///   conversation, and any unread state belongs there.
///
/// `is_tombstoned` must be decided the way `Store::list_rooms` decides to hide —
/// on the *presence* of an `m.room.tombstone` state event, not on whether a
/// successor can be read out of it. A tombstone whose `replacement_room` is
/// missing, or one that has been redacted, still hides the room from every
/// client while yielding no successor; inferring from the successor would leave
/// exactly those rooms accruing an unreadable count forever, which is the bug
/// ADR 0090 exists to fix.
///
/// Nothing tests that, and this function's own tests cannot: by the time a
/// `bool` reaches here, the choice of predicate has already been made at the
/// call site. Swapping `is_tombstoned()` back for `successor_room().is_some()`
/// is green today. Issue #164 tracks the missing seam.
fn unread_suppression_reason(is_gone_upstream: bool, is_tombstoned: bool) -> Option<&'static str> {
    if is_gone_upstream {
        Some("room is absent upstream")
    } else if is_tombstoned {
        Some("room has been tombstoned")
    } else {
        None
    }
}

/// Whether an upstream probe's outcome proves the homeserver does not serve the
/// room (ADR 0090).
///
/// Only a *client* rejection counts — `403` (we are not in a room the server
/// does know) or `404` (it knows no such room or route). Everything else is
/// inconclusive by construction: a `5xx` is the homeserver failing, not
/// answering, and Synapse reports its own internal errors as `M_UNKNOWN` exactly
/// like the `404 M_UNKNOWN` a purged room produces — so classifying on the
/// error *kind* rather than the status would let a server-side outage mark live
/// rooms gone. A transport failure carries no status at all and reaches here as
/// `None`.
fn probe_proves_room_absent(status: Option<u16>) -> bool {
    matches!(status, Some(403) | Some(404))
}

/// What one upstream probe settled (ADR 0090).
///
/// Three outcomes, deliberately not two. An earlier version carried a
/// `bool` "absent", which made `Reachable` and `Inconclusive` the same value and
/// cleared the suspect row for both — so a transient `502` during an outage
/// erased the suspicion on a genuinely purged room, destroyed its
/// `first_flagged_at`, dropped it out of `suspect_upstream_rooms` (only
/// `suspect` rows are re-probed), and logged *"suspect room answered upstream"*
/// while doing it. Nothing but the user re-opening the room would put it back,
/// which is the opposite of the unattended reconcile this ADR promises. Keep
/// these three distinct: "it answered" and "we could not tell" are not the same
/// fact, and only one of them is evidence.
#[derive(Debug, PartialEq, Eq)]
enum ProbeVerdict {
    /// The homeserver served the room. Clears any suspicion.
    Reachable,
    /// A client rejection proves the homeserver does not serve it.
    Absent,
    /// The homeserver failed rather than answered, or never answered. Proves
    /// nothing; the row stays `suspect` for a later pass.
    Inconclusive,
}

/// Classify a probe's result. Split from the probe itself so the three-way
/// decision is testable without a homeserver — the predicate it wraps was always
/// unit-tested, while the branch consuming it was not, which is how the
/// two-outcome version shipped green.
fn probe_verdict<T, E>(outcome: &Result<T, E>, status: Option<u16>) -> ProbeVerdict {
    match outcome {
        Ok(_) => ProbeVerdict::Reachable,
        Err(_) if probe_proves_room_absent(status) => ProbeVerdict::Absent,
        Err(_) => ProbeVerdict::Inconclusive,
    }
}

/// How often [`watch_invites`] re-walks `invited_rooms()` as a self-healing
/// backstop (ADR 0091). Same interval as unread counts: the notable-update
/// receiver is lossy and has no dedicated "invite changed" reason bit.
const INVITES_RESWEEP: Duration = Duration::from_secs(300);

/// Bound on concurrent `capture_invite` calls within one [`sweep_invites`]
/// pass. Pending invites are usually a handful; the cap is for a backlog of
/// unsolicited invites, not a 1755-room joined list.
const INVITES_SWEEP_CONCURRENCY: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnreadCountsSnapshot {
    notification: u64,
    highlight: u64,
}

impl UnreadCountsSnapshot {
    fn new(notification: u64, highlight: u64) -> Self {
        Self {
            notification,
            highlight,
        }
    }

    /// Read a room's current counts, alongside whether matrix-sdk considers its
    /// read-receipt state *settled* — i.e. every receipt it knows about has been
    /// matched to an event in the room's in-memory linked chunk.
    ///
    /// An unmatched receipt lands in `RoomReadReceipts::pending`, and while one
    /// sits there the counts beside it were computed from a *fallback* anchor:
    /// `select_best_receipt` walks the linked chunk for the most recent event it
    /// can treat as a read position, and when the real receipt's target isn't in
    /// the chunk it settles for an older one, then counts everything after it.
    /// That is how a silent room reports fresh unread notifications (see
    /// [`capture_unread_counts`]).
    fn from_room(room: &Room) -> (Self, bool) {
        let read_receipts = room.read_receipts();
        (
            Self::new(read_receipts.num_notifications, read_receipts.num_mentions),
            read_receipts.pending.is_empty(),
        )
    }

    fn pair(self) -> (u64, u64) {
        (self.notification, self.highlight)
    }

    fn clamped(self) -> (i64, i64) {
        (
            saturating_i64(self.notification),
            saturating_i64(self.highlight),
        )
    }
}

fn cached_unread_counts_match(cached: Option<(u64, u64)>, snapshot: UnreadCountsSnapshot) -> bool {
    cached == Some(snapshot.pair())
}

/// Whether `snapshot` raises either count above what was last captured. An
/// absent cache entry counts as zero, so the very first capture of a nonzero
/// value is an increase — that is the case the phantom-count guard in
/// [`capture_unread_counts`] most needs to catch, since a restart that lost the
/// persisted row arrives here with nothing cached.
fn unread_counts_increased(cached: Option<(u64, u64)>, snapshot: UnreadCountsSnapshot) -> bool {
    let (cached_notification, cached_highlight) = cached.unwrap_or((0, 0));
    snapshot.notification > cached_notification || snapshot.highlight > cached_highlight
}

/// Cap on candidate invites auto-joined per [`VERIFICATION_POLL`]. A backlog of
/// direct invites must not translate into an unbounded join + explicit-subscribe
/// spike — the blast radius ADR 0040 exists to avoid. Anything past the cap is
/// left for a later poll. Sized well above the handful of concurrent verifications
/// a real session runs.
const MAX_CANDIDATE_JOINS_PER_POLL: usize = 8;

/// Whether we currently share a joined room with `user_id`. Reads local state only
/// (`get_member_no_sync`, no network) and short-circuits on the first shared room.
///
/// The `Join` membership check matters: `get_member_no_sync` returns a member for
/// *any* user with a stored `m.room.member` event, including ones who have since
/// left or been kicked/banned. Without it, a former co-member would still pass the
/// consent gate and could make this account auto-join a DM by inviting it — the
/// exact bypass the gate exists to prevent.
async fn shares_joined_room(client: &Client, user_id: &UserId) -> bool {
    for room in client.joined_rooms() {
        if matches!(
            room.get_member_no_sync(user_id).await,
            Ok(Some(member)) if *member.membership() == MembershipState::Join
        ) {
            return true;
        }
    }
    false
}

/// Join pending direct invites **from known contacts** and register them as TTL'd
/// candidate verification rooms (ADR 0040). Cross-user verification creates/uses a
/// DM and invites us; until we join, the room's timeline — carrying the
/// `m.key.verification.request` — is never delivered.
///
/// Two gates keep this from reintroducing the blast radius this PR exists to fix,
/// and from being abusable. This mirrors how Element scopes verification: you
/// verify a user from a room you already share, and Element does not silently
/// auto-join an arbitrary invite to receive one.
///
///   * **Known-contact only.** We auto-join a direct invite only when we already
///     share a joined room with the inviter — the only users a verification is
///     meaningful with. This denies an arbitrary user the ability to make this
///     account silently join a DM just by inviting it (the consent concern).
///   * **Per-poll cap.** At most [`MAX_CANDIDATE_JOINS_PER_POLL`] invites are
///     joined per poll, so a backlog can't produce an unbounded join + subscribe
///     spike; the remainder is picked up on subsequent polls.
///
/// Each joined room is still tracked as a short-lived candidate, so a
/// non-verification invite drops out of the explicit subscription set shortly
/// after rather than being parked there forever. An invite that fails the
/// direct/known-contact gates is memoized as rejected, so a standing non-candidate
/// invite doesn't re-run the same checks every poll.
async fn join_candidate_invites(client: &Client, account_id: Uuid, rooms: &VerificationRooms) {
    let mut joined = 0usize;
    for room in client.invited_rooms() {
        if joined >= MAX_CANDIDATE_JOINS_PER_POLL {
            tracing::debug!(
                %account_id,
                "candidate-invite join cap reached this poll; deferring remaining invites"
            );
            break;
        }
        let room_id = room.room_id().to_owned();
        // Skip invites already rejected within the memo window — avoids re-running
        // the membership scan on the same standing non-contact invite every poll.
        if rooms.is_recently_rejected(account_id, &room_id) {
            continue;
        }
        if !room.is_direct().await.unwrap_or(false) {
            rooms.mark_rejected(account_id, room_id);
            continue;
        }
        let inviter_id = match room.invite_details().await {
            Ok(invite) => invite.inviter_id,
            Err(err) => {
                tracing::warn!(%account_id, %room_id, error = %err,
                    "failed to read invite details; skipping candidate invite");
                continue;
            }
        };
        if !shares_joined_room(client, &inviter_id).await {
            tracing::debug!(%account_id, %room_id, %inviter_id,
                "ignoring direct invite from a user we share no room with (not a verification candidate)");
            rooms.mark_rejected(account_id, room_id);
            continue;
        }
        if let Err(err) = client.join_room_by_id(&room_id).await {
            tracing::warn!(%account_id, %room_id, error = %err, "failed to join verification candidate invite");
            continue;
        }
        joined += 1;
        tracing::info!(%account_id, %room_id, %inviter_id,
            "joined direct invite from known contact as verification candidate");
        rooms.add_candidate(account_id, room_id);
    }
}

/// Re-derive the explicit subscription set (active-flow rooms ∪ live candidate
/// invites) and, **only if it changed**, push it to the sliding-sync service (ADR
/// 0040). `subscribe_to_rooms` replaces all prior subscriptions and cancels the
/// in-flight request, so we must not call it when nothing changed — otherwise the
/// 5-second poll would repeatedly disrupt sync. The set is bounded by concurrent
/// verifications plus recently-invited DMs, never the whole DM list.
async fn maybe_resubscribe_verification_rooms(
    rls: &RoomListService,
    registry: &FlowRegistry,
    rooms: &VerificationRooms,
    account_id: Uuid,
    subscribed: &mut HashSet<OwnedRoomId>,
) {
    let mut desired: HashSet<OwnedRoomId> = active_flow_rooms(registry, account_id)
        .into_iter()
        .collect();
    desired.extend(rooms.live_candidates(account_id));
    if desired == *subscribed {
        return;
    }
    let ids: Vec<OwnedRoomId> = desired.iter().cloned().collect();
    let refs: Vec<&RoomId> = ids.iter().map(AsRef::as_ref).collect();
    rls.subscribe_to_rooms(&refs).await;
    tracing::info!(
        %account_id,
        count = refs.len(),
        room_ids = ?ids,
        "updated verification room subscriptions"
    );
    *subscribed = desired;
}

/// Run one account's sync to completion: authenticate, start the sync service,
/// and monitor its state until cancellation (returns `Ok`) or an error/terminal
/// state (returns `Err`, triggering a supervised restart).
#[allow(clippy::too_many_arguments)]
async fn run_account(
    store: &Store,
    config: &SyncConfig,
    account: &Account,
    cancel: &CancellationToken,
    live_tx: &broadcast::Sender<LiveFrame>,
    manager: &ClientManager,
    locks: &IdentityLocks,
    tracker: &TaskTracker,
    verifications: &FlowRegistry,
    verification_rooms: &VerificationRooms,
    index: Option<&IndexHandle>,
    backfill_health: &BackfillHealth,
    sync_health: &SyncHealth,
) -> Result<(), SyncError> {
    // The manager owns client construction + caching (and single-flight with the
    // gateway, which may have connected this account already). A connect failure
    // surfaces as a SyncError so the supervisor's backoff/retry is unchanged.
    let client = manager.get_or_connect(account.account_id).await?;
    // Subscribe before sync can make an authenticated request and trigger token
    // refresh. The watcher's initial full snapshot also heals a refresh made by
    // a gateway that lazily connected this client before the supervisor arrived.
    let oauth_session_changes = (account.auth_kind == AccountAuthKind::OAuth)
        .then(|| client.subscribe_to_session_changes());

    // Register event persistence before starting the sync service so no events
    // are missed between SyncService::start() and handler registration.
    let persist_ctx = PersistContext {
        store: store.clone(),
        account_id: account.account_id,
        live_tx: live_tx.clone(),
        index: index.cloned(),
        local_user_id: Arc::from(account.user_id.as_str()),
        purge_on_leave: config.purge_on_leave,
    };
    // Clone before the handler context takes ownership: the backfill task persists
    // paged events through the same path (M10), reusing this context.
    let backfill_ctx = persist_ctx.clone();
    client.add_event_handler_context(persist_ctx);
    client.add_event_handler(persist_timeline_event);
    // Room state + account data (ADR 0016). These reuse the same PersistContext.
    // The global-account-data handler must not take a `Room` argument — it has no
    // room, and the SDK skips a handler whose `Room` extractor fails.
    client.add_event_handler(persist_room_state_event);
    client.add_event_handler(persist_room_account_data);
    client.add_event_handler(persist_global_account_data);

    // Generic ephemeral passthrough (ADR 0056): forward an allowlisted raw
    // ephemeral room event (m.typing, m.receipt, …) straight onto the live-event
    // bus. Axon derives nothing from these and persists nothing, so this is a
    // plain handler, not a child task — no store/index/lifecycle involvement.
    let ephemeral_allowlist: HashSet<String> =
        config.ephemeral_event_types.iter().cloned().collect();
    // `forward_ephemeral_event` is registered only for room-scoped ephemeral
    // events (matrix-sdk's `HandlerKind::EphemeralRoomData`); `m.presence` is
    // account-scoped and dispatches via a structurally separate handler kind
    // this PR does not register, so it can never be forwarded regardless of
    // this allowlist. Warn loudly rather than let it silently no-op.
    if ephemeral_allowlist.contains("m.presence") {
        tracing::warn!(
            account_id = %account.account_id,
            "m.presence is in sync.ephemeral_event_types but presence is account-scoped and \
             never reaches the room-scoped ephemeral handler; it will not be forwarded until \
             presence gets its own handler registration"
        );
    }
    tracing::debug!(
        account_id = %account.account_id,
        allowlist = ?ephemeral_allowlist,
        "resolved ephemeral passthrough allowlist"
    );
    client.add_event_handler_context(EphemeralCtx {
        account_id: account.account_id,
        live_tx: live_tx.clone(),
        allowlist: Arc::new(ephemeral_allowlist),
    });
    client.add_event_handler(forward_ephemeral_event);

    // Interactive SAS verification (M7a PR6): surface peer-initiated requests with
    // no HTTP kickoff. The handler registers the flow and spawns a driver under a
    // child of this account's cancel token, onto the engine tracker, so it shuts
    // down with the account. Outgoing flows (started via the API) are driven by the
    // VerificationEngine directly; both share `verifications`.
    client.add_event_handler_context(VerificationListenerCtx {
        account_id: account.account_id,
        registry: verifications.clone(),
        live_tx: live_tx.clone(),
        tracker: tracker.clone(),
        cancel: cancel.clone(),
        rooms: verification_rooms.clone(),
        handled_room_events: HandledRoomEvents::default(),
    });
    client.add_event_handler(on_incoming_request);
    // Cross-user verification (ADR 0040) arrives as a room message rather than a
    // to-device event; both handlers share the context above.
    client.add_event_handler(on_incoming_room_request);

    // `SyncService::builder` consumes the client; keep a clone for the
    // re-decryption queue and the startup sweep (the client is Arc-backed, so
    // clones are cheap and share one underlying connection + crypto store).
    // Raise the room-list timeline window from the SDK default of 1 (latest
    // event only) so each room archives its last N events. See ADR 0015.
    //
    // `with_offline_mode` makes `State::Offline` reachable: on a sync failure the
    // SDK itself now retries `GET /_matrix/client/versions` (an unbounded loop,
    // ~100ms between attempts — see `SyncService::offline_check`) and resumes
    // syncing on the same client/session once the homeserver answers, instead of
    // surfacing `State::Error` immediately. That leaves this function's own
    // `State::Error`/`Terminated` handling below, and `supervise_account`'s
    // external backoff-restart above it, in place for what offline mode does not
    // absorb: session-level failures reported through a `TerminationReport` (e.g.
    // an explicit `stop()`), and the state stream closing outright. The two are
    // not in tension — offline mode is the fast, cheap path for "homeserver is
    // briefly unreachable"; the outer restart is the fallback for everything else.
    let sync_service = SyncService::builder(client.clone())
        .with_room_list_timeline_limit(config.timeline_limit)
        .with_offline_mode()
        .build()
        .await
        .map_err(sdk_err)?;

    // OAuth refresh durability is part of this account run's supervised
    // lifetime. One writer per run serializes snapshots, and teardown drains it
    // before the cached client can be revoked or replaced.
    let oauth_session_cancel = cancel.child_token();
    let oauth_session_handle = oauth_session_changes.map(|changes| {
        let store_key = config
            .store_key
            .clone()
            .expect("OAuth client restore requires sync.store_key");
        tokio::spawn(matrix_oauth::watch_session_changes(
            client.clone(),
            store.clone(),
            account.account_id,
            store_key,
            changes,
            oauth_session_cancel.clone(),
        ))
    });
    sync_service.start().await;
    tracing::info!(account_id = %account.account_id, "sync service started");

    // Re-decryption queue: a child token so it ends with this run, and a join
    // handle so we drain it cleanly before returning (or restarting).
    let redecrypt_cancel = cancel.child_token();
    let redecrypt_handle = tokio::spawn(redecrypt::run(
        client.clone(),
        store.clone(),
        account.account_id,
        redecrypt_cancel.clone(),
        index.cloned(),
    ));
    // One sweep now that the service is up and `recover()` (if any) has imported
    // keys: keys already in the crypto store don't fire the arrival stream. By
    // default, each row gets one startup attempt; operators can opt into the
    // legacy every-boot full sweep. Raced against cancellation: a large backlog
    // can take a while (one async decrypt call per row), and the sweep has no
    // cancel-checks of its own, so without this a shutdown mid-sweep would block
    // until the engine's hard drain timeout rather than exiting promptly. Safe to
    // abandon mid-row — rows stay `pending` and are retried on the next boot's
    // sweep (or the room-key arrival stream).
    let scope = if config.always_redecrypt_utds_on_startup {
        redecrypt::SweepScope::AllPending
    } else {
        redecrypt::SweepScope::StartupUnattempted
    };
    tokio::select! {
        _ = cancel.cancelled() => {}
        summary = redecrypt::sweep_pending_utds(&client, store, account.account_id, scope, index) => {
            if summary.selected > 0 {
                tracing::info!(
                    account_id = %account.account_id,
                    selected = summary.selected,
                    attempted = summary.attempted,
                    decrypted = summary.decrypted,
                    still_pending = summary.still_pending,
                    startup_marked = summary.startup_marked,
                    "startup UTD re-decryption sweep completed"
                );
            }
        }
    }

    // History backfill (M10): a continuous, throttled background task that pages
    // each joined room's pre-existing history backward through the shared ingestion
    // path. Same child-token + join-handle lifecycle as the re-decryption queue, so
    // it ends with this run and is drained cleanly below. Gated by config; `None`
    // when disabled so there is nothing to drain.
    let backfill_cancel = cancel.child_token();
    let backfill_handle = if config.backfill_enabled {
        Some(tokio::spawn(backfill::run(
            client.clone(),
            backfill_ctx,
            BackfillParams::from_config(config),
            backfill_health.clone(),
            backfill_cancel.clone(),
        )))
    } else {
        None
    };

    // Verification watcher (ADR 0026): keep the persisted `verified` flag tracking
    // the SDK's current cross-signing state. Same child-token + join-handle
    // lifecycle as the re-decryption queue, so it ends with this run and is drained
    // cleanly below rather than leaking across a supervised restart.
    let verify_cancel = cancel.child_token();
    // The watcher's `verified` write is serialized against the lifecycle verbs
    // (recover/logout) by taking this identity's lock — the *same* lock those verbs
    // hold — so a watcher derive can't clobber a concurrent recover's write (ADR
    // 0026, the lost-update race).
    let verify_lock = lock_for(locks, &account.user_id, &account.homeserver_url);
    let verify_handle = tokio::spawn(watch_verification(
        client.clone(),
        store.clone(),
        account.account_id,
        verify_lock,
        verify_cancel.clone(),
    ));

    // Sender-trust overlay watcher (M7c): push `sender_trust.violation` frames when
    // a sender's identity enters a verification violation. Same child-token +
    // join-handle lifecycle as the watchers above, so it ends with this run and is
    // drained cleanly below. Read-only (no persisted state), so no identity lock.
    let trust_cancel = cancel.child_token();
    let trust_handle = tokio::spawn(watch_sender_trust(
        client.clone(),
        account.account_id,
        live_tx.clone(),
        trust_cancel.clone(),
    ));

    // SDK-derived unread-counts watcher (issue #313, ADR 0070): capture
    // matrix-sdk's read-receipt-based notification/mention counts into
    // `room_unread_counts` so a fresh client load can show a real count
    // without observing a live event first. Same child-token + join-handle
    // lifecycle as the watchers above.
    let unread_cancel = cancel.child_token();
    let unread_handle = tokio::spawn(watch_unread_counts(
        client.clone(),
        store.clone(),
        account.account_id,
        live_tx.clone(),
        unread_cancel.clone(),
    ));

    // Pending-invite watcher (issue #279, ADR 0091): persist
    // `client.invited_rooms()` so `GET /v1/invites` can list rooms that never
    // land in `events`. Same child-token + join-handle lifecycle.
    let invites_cancel = cancel.child_token();
    let invites_handle = tokio::spawn(watch_invites(
        client.clone(),
        store.clone(),
        manager.clone(),
        account.account_id,
        account.user_id.clone(),
        live_tx.clone(),
        invites_cancel.clone(),
    ));

    // Cross-user verification room delivery (ADR 0040): register this run's
    // resubscribe waker, and poll for new candidate invites. The sliding-sync
    // selective window (rank 0..=19) delivers no timeline events to a DM outside
    // it, so a verification request in such a room never reaches the handlers; the
    // explicit subscription (active-flow rooms ∪ candidate invites) forces delivery.
    let rls = sync_service.room_list_service();
    let (verification_room_run_id, mut sub_rx) = verification_rooms.register(account.account_id);
    let mut subscribed: HashSet<OwnedRoomId> = HashSet::new();
    let mut verification_poll = tokio::time::interval(VERIFICATION_POLL);
    verification_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut state = sync_service.state();
    // Whether the last state we saw was `Offline`, so a return to `Running`/`Idle`
    // logs a matching "back online" line rather than silently clearing the flag
    // (see the M10-style `/v1/status` sync-health surface this feeds).
    let mut was_offline = false;

    // ADR 0030 (issue #241): the room-list service's own state is more granular
    // than `SyncService::State` above — `Running` there means "the underlying
    // syncs are started", not "a sync has completed" (it's set synchronously
    // inside `SyncService::start`, before any sync round-trip). The room-list
    // state machine only reaches its own `Running` after `Init` -> `SettingUp`
    // -> `Running`, i.e. after at least one full sliding-sync round trip — the
    // closest signal this SDK version exposes for "the first sync cycle
    // completed", which is what unblocks `room.send()` for encrypted rooms.
    // One-shot per run: once seen, stop polling (the `if !sync_ready` guard
    // disables this branch entirely rather than leaving it a no-op forever).
    //
    // `Subscriber::subscribe` (which `rls.state()` calls) only resolves
    // `.next()` for a version strictly newer than the one observed at
    // subscribe time — it does not replay the current value. The room list
    // can already have reached `Running` by the time we get here (redecrypt
    // sweep + three `tokio::spawn` calls sit between `sync_service.start()`
    // above and this line), in which case `.next()` would wait for a *further*
    // transition that may never come, leaving the account stuck reporting
    // `"syncing"` forever. So check the already-observed current value first.
    let mut rls_state = rls.state();
    let mut sync_ready = matches!(rls_state.get(), RoomListState::Running);

    // Emits an `account.sync_state` WS frame (best-effort — no receivers is
    // fine) for a label transition returned by a `sync_health` call below.
    let emit_sync_state = |sync_state: &'static str| {
        let _ = live_tx.send(
            SyncStateFrame {
                account_id: account.account_id,
                sync_state,
            }
            .into(),
        );
    };

    if sync_ready {
        if let Some(sync_state) = sync_health.mark_ready(account.account_id) {
            emit_sync_state(sync_state);
            tracing::info!(account_id = %account.account_id, "first sync cycle complete; account ready");
        }
    }

    let result = loop {
        tokio::select! {
            _ = cancel.cancelled() => break Ok(()),
            // A flow started/accepted or a candidate was added: recompute the set.
            _ = sub_rx.recv() => {
                maybe_resubscribe_verification_rooms(
                    &rls, verifications, verification_rooms, account.account_id, &mut subscribed,
                ).await;
            }
            // Periodically join fresh verification-DM invites and expire stale
            // candidates from the subscription set.
            _ = verification_poll.tick() => {
                join_candidate_invites(&client, account.account_id, verification_rooms).await;
                maybe_resubscribe_verification_rooms(
                    &rls, verifications, verification_rooms, account.account_id, &mut subscribed,
                ).await;
            }
            next = rls_state.next(), if !sync_ready => {
                if matches!(next, Some(RoomListState::Running)) {
                    sync_ready = true;
                    if let Some(sync_state) = sync_health.mark_ready(account.account_id) {
                        emit_sync_state(sync_state);
                        tracing::info!(account_id = %account.account_id, "first sync cycle complete; account ready");
                    }
                }
            }
            next = state.next() => match &next {
                Some(s @ State::Offline) => {
                    if let Some(sync_state) = sync_health.set(account.account_id, s) {
                        emit_sync_state(sync_state);
                    }
                    if !was_offline {
                        was_offline = true;
                        tracing::warn!(account_id = %account.account_id, "sync service went offline");
                    }
                    continue;
                }
                Some(s @ (State::Running | State::Idle)) => {
                    if let Some(sync_state) = sync_health.set(account.account_id, s) {
                        emit_sync_state(sync_state);
                    }
                    if was_offline {
                        was_offline = false;
                        tracing::info!(account_id = %account.account_id, "sync service back online");
                    }
                    continue;
                }
                Some(s @ State::Error(err)) => {
                    if let Some(sync_state) = sync_health.set(account.account_id, s) {
                        emit_sync_state(sync_state);
                    }
                    break Err(SyncError::Sdk(format!("sync service error: {err}")));
                }
                Some(s @ State::Terminated) => {
                    if let Some(sync_state) = sync_health.set(account.account_id, s) {
                        emit_sync_state(sync_state);
                    }
                    break Err(SyncError::Sdk("sync service terminated".into()));
                }
                // The state stream ended; treat as a terminal condition.
                None => break Err(SyncError::Sdk("sync service state stream closed".into())),
            },
        }
    };
    verification_rooms.unregister(account.account_id, verification_room_run_id);

    // Always drain the service so its SQLite store flushes before we drop it,
    // then stop and join the re-decryption queue so it doesn't outlive this run
    // (which would leak a task or duplicate one across a supervised restart).
    sync_service.stop().await;
    oauth_session_cancel.cancel();
    if let Some(handle) = oauth_session_handle {
        if let Err(err) = handle.await {
            tracing::warn!(
                account_id = %account.account_id,
                error = %err,
                "OAuth session watcher did not shut down cleanly"
            );
        }
    }
    redecrypt_cancel.cancel();
    if let Err(err) = redecrypt_handle.await {
        tracing::warn!(
            account_id = %account.account_id,
            error = %err,
            "re-decryption task did not shut down cleanly"
        );
    }
    backfill_cancel.cancel();
    if let Some(handle) = backfill_handle {
        if let Err(err) = handle.await {
            tracing::warn!(
                account_id = %account.account_id,
                error = %err,
                "backfill task did not shut down cleanly"
            );
        }
    }
    verify_cancel.cancel();
    if let Err(err) = verify_handle.await {
        tracing::warn!(
            account_id = %account.account_id,
            error = %err,
            "verification watcher did not shut down cleanly"
        );
    }
    trust_cancel.cancel();
    if let Err(err) = trust_handle.await {
        tracing::warn!(
            account_id = %account.account_id,
            error = %err,
            "sender-trust watcher did not shut down cleanly"
        );
    }
    unread_cancel.cancel();
    if let Err(err) = unread_handle.await {
        tracing::warn!(
            account_id = %account.account_id,
            error = %err,
            "unread-counts watcher did not shut down cleanly"
        );
    }
    invites_cancel.cancel();
    if let Err(err) = invites_handle.await {
        tracing::warn!(
            account_id = %account.account_id,
            error = %err,
            "invites watcher did not shut down cleanly"
        );
    }

    // Cancel any in-flight SAS verification drivers for this account and wait for
    // them to exit: they hold the SDK client this run is tearing down (or
    // rebuilding) — and with it, the crypto store's whole connection pool — so they
    // must not survive it (GH #242). (Incoming drivers run under this account's
    // token; API-started ones under the engine token, so neither is reliably
    // reached by token cascade on a single-account stop — this sweep covers both.)
    cancel_account_flows(verifications, account.account_id).await;

    result
}

/// Keep the persisted `verified` flag tracking axon's own device verification
/// state (ADR 0026). The SDK's `verification_state()` is a `subscribe_reset`
/// subscriber, so the first poll yields the current value — persisting the
/// initial state (`false` for a fresh unverified device, `true` once
/// `recover`/`verify` has cross-signed it) — and each later change (cross-signing
/// rotated, the device's trust reset) re-derives and re-persists. That is what
/// makes the column track the SDK rather than being written once. Runs until
/// `cancel` fires or the subscription closes; a persist failure is logged and
/// skipped (best-effort, never fatal to the run).
async fn watch_verification(
    client: Client,
    store: Store,
    account_id: Uuid,
    lock: IdentityLock,
    cancel: CancellationToken,
) {
    let mut state = client.encryption().verification_state();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            next = state.next() => match next {
                Some(_) => {
                    // Derive AND persist under the per-identity lock — the same lock
                    // `recover`/`logout` hold (ADR 0026). Without it the two derive+
                    // persist steps could interleave with a concurrent `recover`: the
                    // watcher reads pre-import state (`false`), recover imports keys
                    // and writes `true`, then the watcher writes its stale `false` —
                    // a lost update with no guaranteed later emission to self-heal it.
                    // The shared helper holds the lock across the derive, so the
                    // observation always reflects post-recover state.
                    // Pass `cancel`: a lifecycle verb holds this lock while it drains
                    // (awaits) this very task, so the lock wait MUST be cancellation-
                    // aware or shutdown deadlocks and a detached watcher could later
                    // clobber the verb's reset `verified` (ADR 0026).
                    crate::lifecycle::lock_and_persist_verified(
                        &lock,
                        &client,
                        &store,
                        account_id,
                        &cancel,
                    )
                    .await;
                }
                None => return,
            },
        }
    }
}

/// Watch for sender identity changes and surface verification-violation
/// *transitions* as live `sender_trust.violation` overlay frames (M7c). The
/// per-event `sender_trust` snapshot the read API returns is immutable (what
/// Matrix's evidence said when the event arrived); a *current* identity can later
/// enter — or leave — a violation (the sender's cross-signing key changed). This
/// watcher pushes that fact so a client re-evaluates the affected sender — it
/// names the `user_id`, not per-event diffs; the verification bundle / timeline
/// re-read is the source of truth.
///
/// Subscribes to the SDK's `user_identities_stream` and tracks which senders it
/// has reported as in-violation, so it emits exactly on a *change*: a frame with
/// `verification_violation: true` when a sender enters a violation and one with
/// `false` when it clears — the latter is what lets a client un-badge from the
/// live stream alone. Runs until `cancel` fires or the subscription closes; never
/// persists anything (the snapshot is the durable record).
async fn watch_sender_trust(
    client: Client,
    account_id: Uuid,
    live_tx: broadcast::Sender<LiveFrame>,
    cancel: CancellationToken,
) {
    use futures_util::{pin_mut, StreamExt};
    use matrix_sdk::ruma::OwnedUserId;
    use std::collections::HashSet;

    let stream = match client.encryption().user_identities_stream().await {
        Ok(stream) => stream,
        Err(err) => {
            // Known limitation (GH issue #101): a subscribe failure disables the
            // overlay for the rest of this run with no retry. The bundle read
            // endpoint still reports current trust, so it's degraded, not blind.
            tracing::warn!(%account_id, error = %err, "could not subscribe to identity changes; sender-trust overlay disabled for this run");
            return;
        }
    };
    pin_mut!(stream);
    // Senders we've reported as in-violation, so we emit only on a transition (and
    // a clear emits a single `false` frame rather than going silent).
    let mut in_violation: HashSet<OwnedUserId> = HashSet::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            next = stream.next() => match next {
                Some(updates) => {
                    for identity in updates.new.values().chain(updates.changed.values()) {
                        let user_id = identity.user_id().to_owned();
                        let violation = identity.has_verification_violation();
                        // `insert`/`remove` return whether the set actually changed —
                        // our transition test, so an unrelated identity change for a
                        // sender whose violation state is unchanged emits nothing.
                        let changed = if violation {
                            in_violation.insert(user_id.clone())
                        } else {
                            in_violation.remove(&user_id)
                        };
                        if changed {
                            tracing::debug!(%account_id, %user_id, verification_violation = violation, "sender trust changed");
                            let _ = live_tx.send(LiveFrame::SenderTrustChanged(SenderTrustFrame {
                                account_id,
                                user_id: user_id.as_str().to_owned(),
                                verification_violation: violation,
                            }));
                        }
                    }
                }
                None => return,
            },
        }
    }
}

/// Capture `room`'s current SDK-derived unread counts and persist them if
/// they differ from the last value this watcher wrote (issue #313, ADR 0070).
/// `last` is the watcher's own in-memory cache of what it has already
/// persisted this run — not a re-read of the store — so a room whose counts
/// haven't moved since the last capture is a no-op. Skips rooms that aren't
/// currently joined (an invite/left room has no meaningful unread count here).
///
/// A persist failure is logged and `last` is left unchanged, so the next
/// observation for this room (the next notable update, or the periodic
/// re-sweep) retries rather than silently giving up. The lock over `last` is
/// never held across the `.await` below, so concurrent captures for
/// *different* rooms (see [`sweep_unread_counts`]) never block each other on
/// the DB round trip — only the two brief, synchronous check/insert sections
/// are serialized, and a sweep never captures the same room twice
/// concurrently, so no lost update is possible.
async fn capture_unread_counts(
    room: &Room,
    store: &Store,
    account_id: Uuid,
    live_tx: &broadcast::Sender<LiveFrame>,
    last: &Mutex<HashMap<OwnedRoomId, (u64, u64)>>,
    gone_upstream: &Mutex<HashSet<OwnedRoomId>>,
) {
    if room.state() != RoomState::Joined {
        return;
    }
    let (sdk_snapshot, receipts_settled) = UnreadCountsSnapshot::from_room(room);
    let room_id = room.room_id();
    // A room that can never again be corrected by reading it is pinned to zero
    // rather than skipped (ADR 0090): skipping would leave whatever wrong value
    // is already persisted in place, which is the frozen badge this exists to
    // clear. Zeroing flows through the ordinary write path below, so the row,
    // the dedup cache, and the live frame stay consistent.
    let suppression = unread_suppression_reason(
        gone_upstream
            .lock()
            .expect("unread-counts gone-upstream lock")
            .contains(room_id),
        room.is_tombstoned(),
    );
    let snapshot = if suppression.is_some() {
        UnreadCountsSnapshot::new(0, 0)
    } else {
        sdk_snapshot
    };
    let value = snapshot.pair();
    let cached = last
        .lock()
        .expect("unread-counts cache lock")
        .get(room_id)
        .copied();
    if cached_unread_counts_match(cached, snapshot) {
        return;
    }
    if let Some(reason) = suppression {
        tracing::debug!(
            %account_id,
            %room_id,
            sdk_notification_count = sdk_snapshot.notification,
            reason,
            "pinning unread counts to zero"
        );
    }
    // Never let an unsettled room *raise* a count. While matrix-sdk holds an
    // unmatched receipt for this room its counts come from a fallback anchor
    // (see `UnreadCountsSnapshot::from_room`), so an increase here is an
    // artifact of where that anchor landed rather than anything the user has
    // not read — the phantom badge issue #313's watcher would otherwise
    // persist and broadcast.
    //
    // A *decrease* is still written: it can only mean the SDK found a better
    // anchor, so accepting it lets a room that already carries a phantom count
    // self-correct instead of staying wrong until the receipt matches. The
    // deliberate cost is that a genuine new message arriving while `pending` is
    // non-empty has its count held back; `pending` drains as soon as a receipt
    // matches an event, and `UNREAD_COUNTS_RESWEEP` re-evaluates every room
    // afterwards, so the true value lands within one sweep. Suppressing an
    // increase leaves both the cache and the row untouched, which is what makes
    // that later re-evaluation see a diff and write it.
    if suppression.is_none() && !receipts_settled && unread_counts_increased(cached, snapshot) {
        tracing::debug!(
            %account_id,
            %room_id,
            notification_count = snapshot.notification,
            highlight_count = snapshot.highlight,
            "holding unread-count increase: matrix-sdk has unmatched read receipts for this room"
        );
        return;
    }
    // matrix-sdk's counts are `u64`; Postgres has no unsigned type, so narrow
    // at this boundary via the shared helper, and derive the live frame's
    // values from the *same* clamped number rather than the raw SDK fields, so
    // the DB row and the broadcast frame can't disagree.
    let (notification_count, highlight_count) = snapshot.clamped();
    if let Err(err) = store
        .upsert_room_unread_counts(
            account_id,
            room_id.as_str(),
            notification_count,
            highlight_count,
        )
        .await
    {
        tracing::warn!(%account_id, %room_id, error = %err, "failed to persist unread counts");
        return;
    }
    last.lock()
        .expect("unread-counts cache lock")
        .insert(room_id.to_owned(), value);
    let _ = live_tx.send(LiveFrame::UnreadCountsChanged(UnreadCountsFrame {
        account_id,
        room_id: room_id.as_str().to_owned(),
        notification_count: notification_count as u64,
        highlight_count: highlight_count as u64,
    }));
}

/// Re-walk every currently-joined room and capture each one's unread counts,
/// with up to [`UNREAD_COUNTS_SWEEP_CONCURRENCY`] captures in flight at once
/// so an account with many rooms doesn't serialize one Postgres round trip
/// per room. Used both for the startup sweep and the periodic
/// [`UNREAD_COUNTS_RESWEEP`] backstop.
///
/// With `prune`, also drops stale state for rooms the account is no longer in,
/// both the in-memory dedup cache and the persisted `room_unread_counts` row
/// (PR review, issue #313) — without the latter, a left room's row would sit in
/// the table forever (invisible to `list_rooms`, which already filters left
/// rooms, but growing unbounded for a long-lived account that churns through
/// many rooms over time; previously only `ON DELETE CASCADE` on account
/// deletion cleaned these up). Only the periodic re-sweep passes `true`; see the
/// body for why the startup sweep must not prune.
async fn sweep_unread_counts(
    client: &Client,
    store: &Store,
    account_id: Uuid,
    live_tx: &broadcast::Sender<LiveFrame>,
    last: &Mutex<HashMap<OwnedRoomId, (u64, u64)>>,
    gone_upstream: &Mutex<HashSet<OwnedRoomId>>,
    prune: bool,
) {
    use futures_util::stream::{self, StreamExt};

    let rooms = client.joined_rooms();
    // `client.joined_rooms()` is only authoritative once the SDK has loaded this
    // account's rooms. On the startup sweep it routinely is not — it can return a
    // handful of rooms, or none, while the room list is still hydrating — and
    // pruning against that list deletes rows for rooms the account is very much
    // still in. Observed on a 1755-room account: the startup sweep left five rows
    // standing and the following second re-inserted 758 of them.
    //
    // Pruning is housekeeping (ADR 0070: keep a left room's row from sitting in
    // the table forever — `list_rooms` already filters left rooms, so a stale row
    // is invisible, just unbounded), and it has no reason to run before the room
    // list is trustworthy. So the startup sweep never prunes; only the periodic
    // re-sweep does, five minutes in. The empty-list guard covers the same
    // failure at any later sweep: `delete_stale_room_unread_counts` documents an
    // empty `joined_room_ids` as deleting every row for the account, "correct
    // when the account currently has no joined rooms at all" — but nothing here
    // can tell that apart from a room list that has not loaded, and the two
    // outcomes are wildly asymmetric.
    if prune && !rooms.is_empty() {
        let joined: HashSet<OwnedRoomId> =
            rooms.iter().map(|room| room.room_id().to_owned()).collect();
        last.lock()
            .expect("unread-counts cache lock")
            .retain(|room_id, _| joined.contains(room_id));

        let joined_ids: Vec<String> = joined
            .iter()
            .map(|room_id| room_id.as_str().to_owned())
            .collect();
        if let Err(err) = store
            .delete_stale_room_unread_counts(account_id, &joined_ids)
            .await
        {
            tracing::warn!(%account_id, error = %err, "failed to prune stale unread-count rows");
        }
    }

    stream::iter(rooms)
        .for_each_concurrent(UNREAD_COUNTS_SWEEP_CONCURRENCY, |room| async move {
            capture_unread_counts(&room, store, account_id, live_tx, last, gone_upstream).await;
        })
        .await;
}

/// Seed a fresh watcher's in-memory dedup cache from what's already
/// persisted (PR review, issue #313): without this, `last` starts empty on
/// every process restart/reconnect, so the very first startup sweep would
/// treat every joined room as "changed" and unconditionally re-upsert +
/// re-broadcast it, even when nothing actually changed since the last run.
/// A malformed persisted room id (should never happen — every row is
/// written by this same watcher) is logged and skipped rather than failing
/// the whole seed.
async fn seed_unread_counts_cache(
    store: &Store,
    account_id: Uuid,
) -> HashMap<OwnedRoomId, (u64, u64)> {
    let rows = match store.room_unread_counts(account_id).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(
                %account_id, error = %err,
                "failed to read persisted unread counts; starting dedup cache empty"
            );
            return HashMap::new();
        }
    };
    let mut cache = HashMap::with_capacity(rows.len());
    for (room_id, (notification_count, highlight_count)) in rows {
        let Ok(parsed_room_id) = RoomId::parse(&room_id) else {
            tracing::warn!(%account_id, %room_id, "skipping malformed persisted room id");
            continue;
        };
        let notification_count = u64::try_from(notification_count).unwrap_or(0);
        let highlight_count = u64::try_from(highlight_count).unwrap_or(0);
        cache.insert(parsed_room_id, (notification_count, highlight_count));
    }
    cache
}

/// Read the set of rooms currently confirmed absent upstream (ADR 0090).
///
/// Called to seed the watcher and again at the top of every re-sweep, because
/// the table — not the in-memory set — is the source of truth. Suppression has to
/// be able to *stop*: a successful room-scoped send clears a room's row
/// (`SdkGateway::note_room_reachability`), and an accumulate-only set would keep
/// pinning that room's counts to zero for the rest of the process even though
/// the homeserver serves it again.
///
/// Returns `Err` rather than an empty set on a failed read, because "no room is
/// gone" and "we could not find out" are different answers and only one of them
/// is safe to act on. At the seed there is no prior state to lose, so the caller
/// starts empty; at a re-sweep the same value would drop every confirmed verdict
/// on one transient pool timeout, and the sweep immediately after would write
/// each purged room's stale non-zero SDK snapshot back and broadcast it — the
/// frozen badge this ADR exists to clear, restored for a full re-sweep window.
async fn read_gone_upstream_rooms(
    store: &Store,
    account_id: Uuid,
) -> Result<HashSet<OwnedRoomId>, axon_store::StoreError> {
    let rows = store.rooms_gone_upstream(account_id).await?;
    Ok(rows
        .iter()
        .filter_map(|room_id| match RoomId::parse(room_id) {
            Ok(parsed) => Some(parsed),
            Err(_) => {
                tracing::warn!(%account_id, %room_id, "skipping malformed persisted room id");
                None
            }
        })
        .collect())
}

/// Apply a re-sweep's re-read of the absent-upstream set, keeping the current
/// one if the read failed (ADR 0090).
///
/// Split out so the choice is testable without a store: it is a two-line
/// decision that was wrong once and is invisible in the loop it lives in. "No
/// room is gone" and "we could not find out" arrive here as different values and
/// must stay that way — replacing the set from an `Err` un-suppresses every
/// confirmed room, and the sweep that runs next writes each purged room's stale
/// non-zero snapshot back and broadcasts it.
fn apply_gone_upstream_refresh(
    gone_upstream: &Mutex<HashSet<OwnedRoomId>>,
    account_id: Uuid,
    refreshed: Result<HashSet<OwnedRoomId>, axon_store::StoreError>,
) {
    match refreshed {
        Ok(rooms) => {
            *gone_upstream
                .lock()
                .expect("unread-counts gone-upstream lock") = rooms;
        }
        Err(err) => {
            tracing::warn!(
                %account_id, error = %err,
                "failed to refresh rooms absent upstream; keeping the previous set"
            );
        }
    }
}

/// Verify rooms a failed room-scoped call flagged as suspect, and settle each
/// one (ADR 0090).
///
/// The flag itself proves nothing: the rejection Synapse returns for a purged
/// room (`404 M_UNKNOWN: Could not find event …`) names the *event*, and is
/// indistinguishable from "this particular event is unknown". So each suspect
/// room gets one bounded, room-scoped probe — the smallest state read there is,
/// `m.room.create` — and only a client rejection (`403`/`404`, see
/// [`probe_proves_room_absent`]) settles it as absent. A probe that succeeds
/// clears the suspicion; a homeserver failure or a transport error leaves the
/// row for the next pass, so an outage delays a verdict instead of fabricating
/// one.
///
/// At most [`UPSTREAM_PROBES_PER_SWEEP`] rooms per pass, and the remainder is
/// logged rather than silently dropped.
///
/// Checks `cancel` between probes. The loop is the one place in the unread
/// watcher that can hold the tick for minutes — `UPSTREAM_PROBES_PER_SWEEP`
/// sequential round trips, each up to [`UPSTREAM_PROBE_TIMEOUT`] — and its
/// caller's `select!` cannot observe cancellation until it returns, so without
/// this a shutdown would wait out the whole backlog. Abandoning mid-pass costs
/// nothing: every verdict is written durably as it is reached, and the rooms
/// left unprobed are still `suspect` for the next boot.
async fn reconcile_upstream_rooms(
    client: &Client,
    store: &Store,
    account_id: Uuid,
    cancel: &CancellationToken,
) {
    use matrix_sdk::ruma::api::client::state::get_state_event_for_key;
    use matrix_sdk::ruma::events::StateEventType;

    let suspects = match store.suspect_upstream_rooms(account_id).await {
        Ok(suspects) => suspects,
        Err(err) => {
            tracing::warn!(%account_id, error = %err, "failed to read suspect rooms; skipping reconcile");
            return;
        }
    };
    if suspects.is_empty() {
        return;
    }
    if suspects.len() > UPSTREAM_PROBES_PER_SWEEP {
        tracing::debug!(
            %account_id,
            suspects = suspects.len(),
            probing = UPSTREAM_PROBES_PER_SWEEP,
            "deferring some suspect rooms to the next reconcile pass"
        );
    }

    for room_id in suspects.iter().take(UPSTREAM_PROBES_PER_SWEEP) {
        let Ok(parsed_room_id) = RoomId::parse(room_id) else {
            tracing::warn!(%account_id, %room_id, "skipping malformed persisted room id");
            continue;
        };
        let request = get_state_event_for_key::v3::Request::new(
            parsed_room_id.clone(),
            StateEventType::RoomCreate,
            String::new(),
        );
        // Races cancellation rather than only checking between probes: an
        // in-flight probe can sit for `UPSTREAM_PROBE_TIMEOUT`, and shutdown
        // should not have to wait it out. `biased` so a token already cancelled
        // wins without starting another round trip.
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                tracing::debug!(
                    %account_id,
                    "cancelled mid-reconcile; unprobed rooms stay suspect for the next pass"
                );
                return;
            }
            outcome = tokio::time::timeout(UPSTREAM_PROBE_TIMEOUT, client.send(request)) => outcome,
        };
        let (verdict, detail) = match outcome {
            Ok(result) => {
                let status = result
                    .as_ref()
                    .err()
                    .and_then(|err| err.as_client_api_error())
                    .map(|api_err| api_err.status_code.as_u16());
                let detail = match &result {
                    Ok(_) => "probe succeeded".to_owned(),
                    Err(err) => err.to_string(),
                };
                (probe_verdict(&result, status), detail)
            }
            Err(_elapsed) => {
                tracing::debug!(
                    %account_id, %room_id,
                    timeout_secs = UPSTREAM_PROBE_TIMEOUT.as_secs(),
                    "upstream room probe timed out; leaving the room suspect"
                );
                continue;
            }
        };

        if verdict == ProbeVerdict::Inconclusive {
            // The homeserver failed rather than answered. Leave the row — and
            // its `first_flagged_at` — exactly as it is, the same way the
            // timeout arm above does. Clearing here would erase a genuine
            // suspicion on a transient blip and take the room out of the probe
            // queue, since only `suspect` rows are re-probed.
            tracing::debug!(
                %account_id, %room_id, detail,
                "upstream room probe was inconclusive; leaving the room suspect"
            );
            continue;
        }

        if verdict == ProbeVerdict::Absent {
            let promoted = match store
                .mark_room_upstream_gone(account_id, room_id, &detail)
                .await
            {
                Ok(promoted) => promoted,
                Err(err) => {
                    tracing::warn!(%account_id, %room_id, error = %err, "failed to record a room as absent upstream");
                    continue;
                }
            };
            if !promoted {
                // A room-scoped call succeeded while this probe was in flight
                // and cleared the row. That is newer, stronger evidence than a
                // probe that has already returned, so the verdict is dropped
                // rather than written back over it.
                tracing::debug!(
                    %account_id, %room_id, detail,
                    "discarding an absent verdict: the room proved reachable while the probe ran"
                );
                continue;
            }
            // Nothing is inserted into the watcher's in-memory set here: the
            // caller re-reads it from the table right after this pass, so the
            // durable row stays the only place a verdict lives.
            //
            // `warn`: the room stays in the room list with its history, but it is
            // now inert — nothing sent to it will reach anyone.
            tracing::warn!(
                %account_id, %room_id, detail,
                "room is absent upstream; unread counts for it are pinned to zero"
            );
        } else {
            if let Err(err) = store
                .clear_room_upstream_reconcile(account_id, room_id)
                .await
            {
                tracing::warn!(%account_id, %room_id, error = %err, "failed to clear room reconcile state");
                continue;
            }
            tracing::debug!(%account_id, %room_id, detail, "suspect room answered upstream; suspicion cleared");
        }
    }
}

/// Keep `room_unread_counts` tracking matrix-sdk's client-side unread
/// notification/mention counts (issue #313, ADR 0070). These are derived from
/// synced read-receipt state and match the unread badge semantics Matrix
/// clients expose. This is what lets a freshly loaded/reloaded client show a
/// real unread count immediately, rather than only after observing a live
/// event this session.
///
/// The dedup cache (`last`) is seeded from what's already persisted (see
/// [`seed_unread_counts_cache`]) before anything else runs, so a
/// restart/reconnect's startup sweep only re-upserts and re-broadcasts rooms
/// whose counts actually changed since the last run — not unconditionally
/// every joined room.
///
/// Subscribes to `room_info_notable_update_receiver` *before* running the
/// startup sweep — matching the persist handlers' own "register before
/// starting sync" rule elsewhere in `run_account` — so a notable update that
/// lands mid-sweep is queued on the receiver rather than silently missed; the
/// dedup check in [`capture_unread_counts`] makes replaying it afterward a
/// harmless no-op if the sweep already observed the same value. After the
/// startup sweep, the watcher reacts to *every* `room_info_notable_update_receiver`
/// notification regardless of its `reasons` bitflag — there is no dedicated
/// "notification count changed" reason, so filtering on the existing flags
/// would be non-exhaustive — and dedups on the actual value diff. A `Lagged`
/// gap is not specially recovered: the watcher always re-derives the
/// *current* value rather than diffing the missed notification, so a dropped
/// update for a room self-heals the next time anything about that room
/// changes, backstopped by a periodic full re-sweep. Runs until `cancel`
/// fires or the update stream closes.
async fn watch_unread_counts(
    client: Client,
    store: Store,
    account_id: Uuid,
    live_tx: broadcast::Sender<LiveFrame>,
    cancel: CancellationToken,
) {
    let last: Mutex<HashMap<OwnedRoomId, (u64, u64)>> =
        Mutex::new(seed_unread_counts_cache(&store, account_id).await);
    // Rooms a probe has already confirmed absent upstream (ADR 0090). Seeded from
    // the store for the same reason `last` is: a restart must not re-derive a
    // count for a room it has already ruled out, and the verdict is durable.
    let gone_upstream: Mutex<HashSet<OwnedRoomId>> = Mutex::new(
        read_gone_upstream_rooms(&store, account_id)
            .await
            .unwrap_or_else(|err| {
                // Only safe to swallow here: there is no prior set to lose, and
                // the first re-sweep re-reads it. Erring open means a purged
                // room's badge stays wrong until then, which beats pinning a
                // live room to zero on a verdict we never actually read.
                tracing::warn!(
                    %account_id, error = %err,
                    "failed to read rooms absent upstream; starting that set empty"
                );
                HashSet::new()
            }),
    );

    let mut updates = client.room_info_notable_update_receiver();
    sweep_unread_counts(
        &client,
        &store,
        account_id,
        &live_tx,
        &last,
        &gone_upstream,
        false,
    )
    .await;

    // The first tick of `interval` fires immediately; defer it by one period
    // so the resweep timer doesn't immediately re-run what the startup sweep
    // above just did.
    let mut resweep = tokio::time::interval_at(
        tokio::time::Instant::now() + UNREAD_COUNTS_RESWEEP,
        UNREAD_COUNTS_RESWEEP,
    );
    resweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = resweep.tick() => {
                // Probe first, then re-read the verdicts, then sweep: a room
                // confirmed gone in this pass is zeroed by the sweep that
                // follows rather than waiting five more minutes, and a room
                // whose row was cleared since the last pass stops being
                // suppressed in the same window.
                reconcile_upstream_rooms(&client, &store, account_id, &cancel).await;
                // `reconcile_upstream_rooms` returns on cancellation rather than
                // finishing its backlog, so re-check before starting the two
                // steps that follow: a store read plus a full sweep of up to
                // `UNREAD_COUNTS_SWEEP_CONCURRENCY` concurrent writes. Without
                // this, a shutdown signalled mid-probe still waits both of them
                // out before the outer `select!` gets to look at the token
                // again, which is the delay probing races to avoid.
                if cancel.is_cancelled() {
                    return;
                }
                // Only on a successful read; a failed one keeps the previous
                // set. See `apply_gone_upstream_refresh`.
                apply_gone_upstream_refresh(
                    &gone_upstream,
                    account_id,
                    read_gone_upstream_rooms(&store, account_id).await,
                );
                sweep_unread_counts(&client, &store, account_id, &live_tx, &last, &gone_upstream, true)
                    .await;
            }
            update = updates.recv() => match update {
                Ok(update) => {
                    if let Some(room) = client.get_room(&update.room_id) {
                        capture_unread_counts(
                            &room, &store, account_id, &live_tx, &last, &gone_upstream,
                        )
                        .await;
                    }
                }
                // We always re-derive the current value rather than diff the missed
                // notification, so a lagged gap is self-healing — see the doc comment
                // above — and needs no special recovery beyond the periodic re-sweep.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

/// Keep `room_invites` tracking matrix-sdk's invited rooms (issue #279, ADR
/// 0091). Invited rooms never land in `events`, so this watcher is the only
/// write path that makes them listable. Dedup cache is seeded from what's
/// already persisted so a restart does not re-broadcast every standing invite.
///
/// Subscribe before the startup sweep so a notable update mid-sweep is
/// queued rather than missed. React to every notable update (no dedicated
/// invite reason bit) and dedup on the display snapshot. A `Lagged` gap is
/// self-healing via the next update or the periodic re-sweep.
async fn watch_invites(
    client: Client,
    store: Store,
    manager: ClientManager,
    account_id: Uuid,
    account_user_id: String,
    live_tx: broadcast::Sender<LiveFrame>,
    cancel: CancellationToken,
) {
    let last: Mutex<InvitesCache> = Mutex::new(seed_invites_cache(&store, account_id).await);

    let mut updates = client.room_info_notable_update_receiver();
    sweep_invites(
        &client,
        &store,
        &manager,
        account_id,
        &account_user_id,
        &live_tx,
        &last,
    )
    .await;

    let mut resweep = tokio::time::interval_at(
        tokio::time::Instant::now() + INVITES_RESWEEP,
        INVITES_RESWEEP,
    );
    resweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = resweep.tick() => {
                sweep_invites(
                    &client,
                    &store,
                    &manager,
                    account_id,
                    &account_user_id,
                    &live_tx,
                    &last,
                )
                .await;
            }
            update = updates.recv() => match update {
                Ok(update) => {
                    if let Some(room) = client.get_room(&update.room_id) {
                        capture_invite(
                            &room,
                            &store,
                            &manager,
                            account_id,
                            &account_user_id,
                            &live_tx,
                            &last,
                        )
                        .await;
                    }
                    // `get_room` returning None is not "invite withdrawn" —
                    // the room list may not have hydrated this id yet (ADR 0091).
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

/// The watcher's view of `room_invites`: what it has already persisted and
/// announced, so a repeat of the same snapshot is neither re-written nor
/// re-broadcast.
///
/// `seeded` records whether `rows` is known to mirror the table. Until it
/// does, a cache miss means "we don't know", not "there is no row" — reading
/// it as absence is what would let a failed seed disable pruning and let a
/// row deleted out of band stay invisible (ADR 0091).
#[derive(Default)]
struct InvitesCache {
    seeded: bool,
    rows: HashMap<OwnedRoomId, RoomInviteSnapshot>,
}

async fn seed_invites_cache(store: &Store, account_id: Uuid) -> InvitesCache {
    match store.room_invite_snapshots(account_id).await {
        Ok(rows) => InvitesCache {
            seeded: true,
            rows: parse_invite_rows(account_id, rows),
        },
        Err(err) => {
            tracing::warn!(
                %account_id, error = %err,
                "failed to read persisted invites; dedup cache starts unseeded \
                 and the first sweep re-reads it"
            );
            InvitesCache::default()
        }
    }
}

fn parse_invite_rows(
    account_id: Uuid,
    rows: Vec<(String, RoomInviteSnapshot)>,
) -> HashMap<OwnedRoomId, RoomInviteSnapshot> {
    let mut cache = HashMap::with_capacity(rows.len());
    for (room_id, snapshot) in rows {
        let Ok(parsed) = RoomId::parse(&room_id) else {
            tracing::warn!(%account_id, %room_id, "skipping malformed persisted invite room id");
            continue;
        };
        cache.insert(parsed, snapshot);
    }
    cache
}

/// Drop a cached invite whose room is no longer `Invited`. Used when a
/// notable update or sweep sees `get_room` return a room whose
/// `state() != Invited`. `get_room` returning `None` is **not** treated
/// as withdrawn here (ADR 0091: that is a hydration miss, not absence).
async fn drop_invite_if_known(
    store: &Store,
    account_id: Uuid,
    room_id: &RoomId,
    live_tx: &broadcast::Sender<LiveFrame>,
    last: &Mutex<InvitesCache>,
) {
    {
        // Only a cache known to mirror the table can rule the delete out.
        // While unseeded, fall through and let the store answer.
        let cache = last.lock().expect("invites cache lock");
        if cache.seeded && !cache.rows.contains_key(room_id) {
            return;
        }
    }
    match store.delete_room_invite(account_id, room_id.as_str()).await {
        Ok(deleted) => {
            last.lock()
                .expect("invites cache lock")
                .rows
                .remove(room_id);
            if deleted {
                tracing::info!(%account_id, %room_id, "dropped pending invite");
                let _ = live_tx.send(LiveFrame::InviteRemoved(InviteRemovedFrame {
                    account_id,
                    room_id: room_id.as_str().to_owned(),
                }));
            }
        }
        Err(err) => {
            tracing::warn!(%account_id, %room_id, error = %err, "failed to delete pending invite");
        }
    }
}

async fn capture_invite(
    room: &Room,
    store: &Store,
    manager: &ClientManager,
    account_id: Uuid,
    account_user_id: &str,
    live_tx: &broadcast::Sender<LiveFrame>,
    last: &Mutex<InvitesCache>,
) {
    let room_id = room.room_id().to_owned();
    if room.state() != RoomState::Invited {
        drop_invite_if_known(store, account_id, &room_id, live_tx, last).await;
        return;
    }

    // The homeserver has told us it does not know this room (ADR 0094). The
    // SDK offers no way to evict a room from its in-memory list, so it will
    // keep reporting `Invited` until the process restarts — re-persisting the
    // row every sweep and undoing the reject the user just performed. This is
    // the one case where absence of the room upstream is *positive* evidence,
    // established per-room by a `404`, so ADR 0091's guardrail is satisfied.
    if manager.is_room_dead(account_id, &room_id) {
        drop_invite_if_known(store, account_id, &room_id, live_tx, last).await;
        return;
    }

    let details = match room.invite_details().await {
        Ok(details) => details,
        Err(err) => {
            tracing::warn!(
                %account_id,
                %room_id,
                error = %err,
                "failed to read invite details; leaving persisted row untouched"
            );
            return;
        }
    };

    let is_direct = match room.is_direct().await {
        Ok(is_direct) => is_direct,
        Err(err) => {
            tracing::warn!(
                %account_id,
                %room_id,
                error = %err,
                "failed to read whether the invite is a DM; leaving persisted row untouched"
            );
            return;
        }
    };

    let snapshot = RoomInviteSnapshot {
        name: room.name(),
        avatar_url: room.avatar_url().map(|url| url.to_string()),
        topic: room.topic(),
        canonical_alias: room.canonical_alias().map(|alias| alias.to_string()),
        room_type: room.room_type().map(|room_type| room_type.to_string()),
        inviter_user_id: details.inviter_id.as_str().to_owned(),
        inviter_display_name: details
            .inviter
            .as_ref()
            .and_then(|member| member.display_name().map(str::to_owned)),
        is_direct,
        encrypted: room.encryption_state().is_encrypted(),
    };

    {
        // An unseeded cache cannot prove the row is still there, so re-upsert
        // rather than trust a match. Skipping the write when the row has in
        // fact been deleted is what would strand a still-pending invite.
        let cache = last.lock().expect("invites cache lock");
        if cache.seeded && cache.rows.get(&room_id) == Some(&snapshot) {
            return;
        }
    }

    let invited_at = match store
        .upsert_room_invite(account_id, room_id.as_str(), &snapshot)
        .await
    {
        Ok(invited_at) => invited_at,
        Err(err) => {
            tracing::warn!(%account_id, %room_id, error = %err, "failed to persist pending invite");
            return;
        }
    };

    last.lock()
        .expect("invites cache lock")
        .rows
        .insert(room_id.clone(), snapshot.clone());

    tracing::info!(
        %account_id,
        %room_id,
        inviter_user_id = %snapshot.inviter_user_id,
        "persisted pending invite"
    );
    let _ = live_tx.send(LiveFrame::InviteAdded(InviteAddedFrame {
        account_id,
        account_user_id: account_user_id.to_owned(),
        room_id: room_id.as_str().to_owned(),
        name: snapshot.name,
        avatar_url: snapshot.avatar_url,
        topic: snapshot.topic,
        canonical_alias: snapshot.canonical_alias,
        room_type: snapshot.room_type,
        inviter_user_id: snapshot.inviter_user_id,
        inviter_display_name: snapshot.inviter_display_name,
        is_direct: snapshot.is_direct,
        encrypted: snapshot.encrypted,
        invited_at,
    }));
}

/// Re-walk currently-invited rooms and capture each snapshot, and drop rows
/// for rooms we can *positively* see are no longer invited (ADR 0091):
/// `get_room` returned and `state() != Invited`.
///
/// Absence from `invited_rooms()` is deliberately not a prune signal.
/// `invited_rooms()` is `rooms_filtered(INVITED)` over the SDK's in-memory
/// room store — the same partial knowledge that makes `get_room` return
/// `None` for a still-pending invite. A non-empty list is no proof the list
/// is *complete*, so pruning against it can delete valid invites whenever
/// the SDK has hydrated some but not all of them; the row is only re-created
/// later with a fresh `invited_at`, re-sorting the user's inbox. Every delete
/// here needs per-room evidence instead.
async fn sweep_invites(
    client: &Client,
    store: &Store,
    manager: &ClientManager,
    account_id: Uuid,
    account_user_id: &str,
    live_tx: &broadcast::Sender<LiveFrame>,
    last: &Mutex<InvitesCache>,
) {
    use futures_util::stream::{self, StreamExt};

    // Reconcile the dedup cache against what is actually persisted, before
    // anything reads it. This is the watcher's recovery path for both a
    // failed seed and rows deleted out of band — `POST .../leave` writes
    // `room_invites` directly, and a stale cache entry would otherwise
    // suppress the re-persist for the life of the process.
    match store.room_invite_snapshots(account_id).await {
        Ok(rows) => {
            let mut cache = last.lock().expect("invites cache lock");
            cache.rows = parse_invite_rows(account_id, rows);
            cache.seeded = true;
        }
        Err(err) => {
            tracing::warn!(
                %account_id, error = %err,
                "failed to re-read persisted invites; sweeping against a stale cache"
            );
        }
    }

    // Positive-absence: any persisted room the SDK now reports as joined or
    // left is no longer an invite. Safe on the startup sweep too — `get_room`
    // returning a non-invited room is not a hydration miss.
    let known_ids: Vec<OwnedRoomId> = last
        .lock()
        .expect("invites cache lock")
        .rows
        .keys()
        .cloned()
        .collect();
    for room_id in known_ids {
        match client.get_room(&room_id) {
            Some(room) if room.state() != RoomState::Invited => {
                drop_invite_if_known(store, account_id, &room_id, live_tx, last).await;
            }
            Some(_) | None => {}
        }
    }

    // Rooms the homeserver denied knowing (ADR 0094). A failed *accept* learns
    // this without going anywhere near `room_invites`, so the drop has to
    // happen here rather than in the join route — which is also what emits the
    // `invite.removed` frame for it.
    for room_id in manager.dead_rooms_for(account_id) {
        drop_invite_if_known(store, account_id, &room_id, live_tx, last).await;
    }

    stream::iter(client.invited_rooms())
        .for_each_concurrent(INVITES_SWEEP_CONCURRENCY, |room| async move {
            capture_invite(
                &room,
                store,
                manager,
                account_id,
                account_user_id,
                live_tx,
                last,
            )
            .await;
        })
        .await;
}

#[cfg(test)]
mod unread_counts_tests {
    use super::{
        cached_unread_counts_match, saturating_i64, unread_counts_increased, UnreadCountsSnapshot,
    };

    #[test]
    fn unread_snapshot_keeps_notification_and_highlight_together() {
        let snapshot = UnreadCountsSnapshot::new(14, 2);

        assert_eq!(
            snapshot,
            UnreadCountsSnapshot {
                notification: 14,
                highlight: 2,
            }
        );
    }

    #[test]
    fn unread_snapshot_pair_is_the_watchers_dedup_value() {
        let snapshot = UnreadCountsSnapshot::new(3, 1);

        assert_eq!(snapshot.pair(), (3, 1));
    }

    #[test]
    fn unread_snapshot_clamps_the_persisted_values() {
        let snapshot = UnreadCountsSnapshot::new(u64::MAX, 7);

        assert_eq!(snapshot.clamped(), (i64::MAX, 7));
        assert_eq!(saturating_i64(8), 8);
    }

    #[test]
    fn cached_unread_counts_match_only_when_both_counts_match() {
        let cached = Some((3, 1));

        assert!(cached_unread_counts_match(
            cached,
            UnreadCountsSnapshot::new(3, 1)
        ));
        assert!(!cached_unread_counts_match(
            cached,
            UnreadCountsSnapshot::new(4, 1)
        ));
        assert!(!cached_unread_counts_match(
            cached,
            UnreadCountsSnapshot::new(3, 2)
        ));
        assert!(!cached_unread_counts_match(
            None,
            UnreadCountsSnapshot::new(0, 0)
        ));
    }

    #[test]
    fn unread_counts_increased_when_either_count_rises() {
        let cached = Some((3, 1));

        assert!(unread_counts_increased(
            cached,
            UnreadCountsSnapshot::new(4, 1)
        ));
        assert!(unread_counts_increased(
            cached,
            UnreadCountsSnapshot::new(3, 2)
        ));
        assert!(!unread_counts_increased(
            cached,
            UnreadCountsSnapshot::new(3, 1)
        ));
    }

    #[test]
    fn unread_counts_decrease_is_not_an_increase() {
        // The phantom guard must still let a room self-correct downwards while
        // matrix-sdk holds an unmatched receipt, so a count that only falls is
        // never treated as an increase — including all the way to zero.
        let cached = Some((32, 4));

        assert!(!unread_counts_increased(
            cached,
            UnreadCountsSnapshot::new(0, 0)
        ));
        assert!(!unread_counts_increased(
            cached,
            UnreadCountsSnapshot::new(5, 4)
        ));
        // A mixed move still counts as an increase: the risen count is the one
        // that would surface a phantom badge.
        assert!(unread_counts_increased(
            cached,
            UnreadCountsSnapshot::new(5, 9)
        ));
    }

    #[test]
    fn unread_counts_treat_a_missing_cache_entry_as_zero() {
        // The restart case: the persisted row is gone (or was never written),
        // so the first nonzero value the SDK reports must read as an increase —
        // otherwise a fallback-anchor count of 32 in a silent room would be
        // written as if it were the room's true state.
        assert!(unread_counts_increased(
            None,
            UnreadCountsSnapshot::new(32, 0)
        ));
        assert!(!unread_counts_increased(
            None,
            UnreadCountsSnapshot::new(0, 0)
        ));
    }
}

#[cfg(test)]
mod ephemeral_tests {
    use super::ephemeral_frame_from_raw;
    use std::collections::HashSet;
    use uuid::Uuid;

    fn allowlist() -> HashSet<String> {
        ["m.typing", "m.receipt"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn allowlisted_type_is_forwarded_with_its_raw_content() {
        let account_id = Uuid::new_v4();
        let raw = serde_json::json!({
            "type": "m.typing",
            "content": { "user_ids": ["@alice:localhost"] },
        });
        let expected_content = raw["content"].clone();
        let frame = ephemeral_frame_from_raw(account_id, Some("!r:localhost"), raw, &allowlist())
            .expect("m.typing is allowlisted");
        assert_eq!(frame.account_id, account_id);
        assert_eq!(frame.room_id.as_deref(), Some("!r:localhost"));
        assert_eq!(frame.event_type, "m.typing");
        assert_eq!(frame.content, expected_content);
    }

    #[test]
    fn non_allowlisted_type_is_dropped() {
        let raw = serde_json::json!({
            "type": "m.presence",
            "content": { "presence": "online" },
        });
        assert!(ephemeral_frame_from_raw(Uuid::new_v4(), None, raw, &allowlist()).is_none());
    }

    #[test]
    fn empty_allowlist_drops_everything() {
        let raw = serde_json::json!({
            "type": "m.typing",
            "content": { "user_ids": [] },
        });
        assert!(ephemeral_frame_from_raw(
            Uuid::new_v4(),
            Some("!r:localhost"),
            raw,
            &HashSet::new()
        )
        .is_none());
    }

    #[test]
    fn missing_type_or_content_is_dropped() {
        let allow = allowlist();
        // No `type` defaults to "unknown" (matching persist_account_data's
        // convention), which then fails the allowlist check rather than
        // bailing early — same end result, different code path.
        let no_type = serde_json::json!({ "content": {} });
        assert!(ephemeral_frame_from_raw(Uuid::new_v4(), None, no_type, &allow).is_none());

        let no_content = serde_json::json!({ "type": "m.typing" });
        assert!(ephemeral_frame_from_raw(Uuid::new_v4(), None, no_content, &allow).is_none());
    }

    #[test]
    fn m_presence_in_config_is_never_reachable_via_the_room_scoped_helper() {
        // ephemeral_frame_from_raw itself has no opinion on presence — this
        // documents the *actual* gap (finding: forward_ephemeral_event is
        // only ever registered for room-scoped ephemeral events, so no raw
        // presence JSON ever reaches this function in production, regardless
        // of the allowlist). If m.presence is allowlisted, this helper alone
        // would happily build a frame for it — the real fix is the run_account
        // registration warning, not a change here.
        let raw = serde_json::json!({ "type": "m.presence", "content": { "presence": "online" } });
        let allow: HashSet<String> = ["m.presence".to_owned()].into_iter().collect();
        assert!(ephemeral_frame_from_raw(Uuid::new_v4(), None, raw, &allow).is_some());
    }
}

#[cfg(test)]
mod upstream_reconcile_tests {
    use std::collections::HashSet;
    use std::sync::Mutex;

    use matrix_sdk::ruma::{OwnedRoomId, RoomId};
    use uuid::Uuid;

    use super::{
        apply_gone_upstream_refresh, probe_proves_room_absent, probe_verdict,
        unread_suppression_reason, ProbeVerdict,
    };

    /// A healthy joined room is never suppressed — the whole point of the guard
    /// is that it fires only for rooms whose counts can no longer be corrected.
    #[test]
    fn a_healthy_room_is_not_suppressed() {
        assert!(unread_suppression_reason(false, false).is_none());
    }

    /// Both conditions suppress on their own, and the absent-upstream reason wins
    /// when a room is somehow both (the more specific fact about why nothing can
    /// reach it).
    #[test]
    fn absent_or_tombstoned_suppresses() {
        assert_eq!(
            unread_suppression_reason(true, false),
            Some("room is absent upstream")
        );
        assert_eq!(
            unread_suppression_reason(false, true),
            Some("room has been tombstoned")
        );
        assert_eq!(
            unread_suppression_reason(true, true),
            Some("room is absent upstream")
        );
    }

    /// Only a client rejection settles a room as absent.
    #[test]
    fn client_rejections_prove_a_room_absent() {
        assert!(probe_proves_room_absent(Some(403)));
        assert!(probe_proves_room_absent(Some(404)));
    }

    /// A homeserver that fails rather than answers proves nothing. This is the
    /// case that matters most: Synapse reports its own internal errors with the
    /// same `M_UNKNOWN` errcode a purged room produces, so a 500 during an
    /// outage must not mark every probed room gone.
    #[test]
    fn server_failures_and_transport_errors_prove_nothing() {
        assert!(!probe_proves_room_absent(Some(500)));
        assert!(!probe_proves_room_absent(Some(502)));
        assert!(!probe_proves_room_absent(Some(429)));
        assert!(!probe_proves_room_absent(None));
    }

    /// A success is not a rejection, however the caller reports it.
    #[test]
    fn success_statuses_prove_nothing() {
        assert!(!probe_proves_room_absent(Some(200)));
    }

    /// "It answered" and "we could not tell" are different facts, and only the
    /// first is evidence. The two-outcome version of this code collapsed them
    /// into one `bool` and cleared a genuine suspicion on any 5xx.
    #[test]
    fn an_inconclusive_probe_is_not_a_success() {
        let failed: Result<(), ()> = Err(());
        for status in [Some(500), Some(502), Some(429), None] {
            assert_eq!(
                probe_verdict(&failed, status),
                ProbeVerdict::Inconclusive,
                "status {status:?} proves nothing and must not read as reachable"
            );
        }
    }

    /// Only a client rejection settles a room as absent.
    #[test]
    fn a_client_rejection_settles_the_room_absent() {
        let failed: Result<(), ()> = Err(());
        assert_eq!(probe_verdict(&failed, Some(403)), ProbeVerdict::Absent);
        assert_eq!(probe_verdict(&failed, Some(404)), ProbeVerdict::Absent);
    }

    fn room(id: &str) -> OwnedRoomId {
        RoomId::parse(id).expect("test room id")
    }

    /// A failed re-read is not evidence that nothing is gone.
    ///
    /// The set is replaced on every re-sweep, so a store error that produced an
    /// empty set here would un-suppress every confirmed room on one transient
    /// pool timeout — and the sweep that runs immediately after would write each
    /// purged room's stale non-zero snapshot back and broadcast it, restoring
    /// the frozen badge ADR 0090 exists to clear for a whole re-sweep window.
    #[test]
    fn a_failed_refresh_keeps_the_previous_gone_set() {
        let gone: Mutex<HashSet<OwnedRoomId>> =
            Mutex::new(HashSet::from([room("!purged:localhost")]));

        apply_gone_upstream_refresh(
            &gone,
            Uuid::new_v4(),
            Err(axon_store::StoreError::EmbeddedMigration(
                "pool timeout".to_owned(),
            )),
        );

        assert_eq!(
            *gone.lock().unwrap(),
            HashSet::from([room("!purged:localhost")]),
            "a read that failed must not be read as an empty verdict set"
        );
    }

    /// A successful re-read *does* replace it — including with an empty set,
    /// which is how a recovered room stops being suppressed without a restart.
    #[test]
    fn a_successful_refresh_replaces_the_gone_set() {
        let gone: Mutex<HashSet<OwnedRoomId>> =
            Mutex::new(HashSet::from([room("!recovered:localhost")]));

        apply_gone_upstream_refresh(&gone, Uuid::new_v4(), Ok(HashSet::new()));

        assert!(
            gone.lock().unwrap().is_empty(),
            "clearing a room's row must un-suppress it in the same window"
        );
    }

    /// A served room clears its suspicion, and the status carried alongside a
    /// success never overrides that.
    #[test]
    fn a_served_room_is_reachable() {
        let served: Result<(), ()> = Ok(());
        assert_eq!(probe_verdict(&served, None), ProbeVerdict::Reachable);
        assert_eq!(probe_verdict(&served, Some(200)), ProbeVerdict::Reachable);
        assert_eq!(
            probe_verdict(&served, Some(404)),
            ProbeVerdict::Reachable,
            "the outcome decides, not a stray status"
        );
    }
}

/// Probe-classification tests that drive the real loop against a stand-in
/// homeserver (ADR 0090).
///
/// The three-way verdict is unit-tested above, but the bug that motivated it was
/// in the *branch consuming it*, not the predicate — so these run
/// [`reconcile_upstream_rooms`] end to end against an axum loopback server and
/// assert what happened to the durable row. DB-gated like the rest of the
/// store-touching tests.
#[cfg(test)]
mod reconcile_loop_tests {
    use std::net::SocketAddr;

    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    use super::*;

    /// Serve `router` on an ephemeral loopback port, mirroring `discovery.rs`'s
    /// harness. The task lives for the duration of the test process.
    async fn serve(router: Router) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        addr
    }

    /// A homeserver that answers `status` to the probe.
    ///
    /// A fallback rather than a route for the state endpoint: ruma sends the
    /// empty `state_key` as a trailing empty path segment
    /// (`…/state/m.room.create/`), which an obvious `{state_key}` pattern does
    /// not match — the router would 404 on its own and every test would read as
    /// a client rejection no matter what it configured. Answering everything
    /// keeps the stand-in honest about the one thing it is standing in for.
    ///
    /// `restore_session`'s account-data read is excepted and always 404s, which
    /// is what a homeserver says for account data that was never set; letting it
    /// fall through would hand the client a room-create body for it.
    fn state_router(status: StatusCode, body: serde_json::Value) -> Router {
        Router::new()
            .route(
                "/_matrix/client/v3/user/{user_id}/account_data/{event_type}",
                get(|| async {
                    (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "errcode": "M_NOT_FOUND", "error": "not set" })),
                    )
                }),
            )
            .fallback(move || {
                let body = body.clone();
                async move { (status, Json(body)) }
            })
    }

    /// A client that will actually *send* the probe.
    ///
    /// The session restore is load-bearing, not ceremony: the room-state
    /// endpoint requires authentication, so an anonymous client fails locally
    /// with "no access token provided" and never opens a connection. That error
    /// carries no status, so it classifies as `Inconclusive` — which is the
    /// verdict two of these tests are asserting, and they would pass without the
    /// homeserver being consulted at all.
    async fn client_for(addr: SocketAddr) -> Client {
        use matrix_sdk::authentication::matrix::MatrixSession;
        use matrix_sdk::store::RoomLoadSettings;
        use matrix_sdk::{SessionMeta, SessionTokens};

        let client = Client::builder()
            .homeserver_url(format!("http://{addr}"))
            .server_versions([matrix_sdk::ruma::api::MatrixVersion::V1_11])
            // Retry is disabled for the same reason the session is restored: with
            // it on, matrix-sdk retries a 5xx until `UPSTREAM_PROBE_TIMEOUT`
            // elapses, so the loop takes the *timeout* arm and the inconclusive
            // branch under test is never reached. That version of this test
            // passed against the two-outcome bug it exists to catch, and took 15
            // seconds to do it.
            .request_config(matrix_sdk::config::RequestConfig::new().disable_retry())
            .build()
            .await
            .expect("client against the stand-in homeserver");
        client
            .matrix_auth()
            .restore_session(
                MatrixSession {
                    meta: SessionMeta {
                        user_id: matrix_sdk::ruma::UserId::parse("@probe:localhost")
                            .expect("test user id"),
                        device_id: "PROBEDEV".into(),
                    },
                    tokens: SessionTokens {
                        access_token: "probe-token".to_owned(),
                        refresh_token: None,
                    },
                },
                RoomLoadSettings::default(),
            )
            .await
            .expect("restore a session so the probe is actually sent");
        client
    }

    async fn test_store() -> Store {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for these tests");
        Store::connect(&url, 5).await.expect("connect + migrate")
    }

    /// A homeserver that fails rather than answers must **delay** a verdict, not
    /// erase the suspicion.
    ///
    /// This is the case the two-outcome version got wrong: `502` is not a client
    /// rejection, so it was classified "not absent", fell into the same branch as
    /// a success, and cleared the row — dropping a genuinely purged room out of
    /// `suspect_upstream_rooms`, which is the only queue that gets re-probed.
    /// Reverting to that behaviour fails this test.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn an_inconclusive_probe_leaves_the_room_suspect() {
        let store = test_store().await;
        let account = store
            .upsert_account(
                &format!("@probe-502-{}:localhost", Uuid::new_v4()),
                "https://hs.example.org",
            )
            .await
            .expect("account");
        let account_id = account.account_id;
        let room_id = format!("!inconclusive-{}:localhost", Uuid::new_v4());

        store
            .flag_room_upstream_suspect(account_id, &room_id, "M_FORBIDDEN")
            .await
            .expect("flag suspect");

        let addr = serve(state_router(
            StatusCode::BAD_GATEWAY,
            json!({ "errcode": "M_UNKNOWN", "error": "bad gateway" }),
        ))
        .await;
        reconcile_upstream_rooms(
            &client_for(addr).await,
            &store,
            account_id,
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(
            store
                .suspect_upstream_rooms(account_id)
                .await
                .expect("suspects"),
            vec![room_id.clone()],
            "an outage delays the verdict; it must not erase the suspicion"
        );
        assert!(
            store
                .rooms_gone_upstream(account_id)
                .await
                .expect("gone")
                .is_empty(),
            "nor does it fabricate one"
        );
    }

    /// A client rejection is the only thing that settles a room as absent.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn a_rejected_probe_settles_the_room_absent() {
        let store = test_store().await;
        let account = store
            .upsert_account(
                &format!("@probe-404-{}:localhost", Uuid::new_v4()),
                "https://hs.example.org",
            )
            .await
            .expect("account");
        let account_id = account.account_id;
        let room_id = format!("!purged-{}:localhost", Uuid::new_v4());

        store
            .flag_room_upstream_suspect(account_id, &room_id, "M_FORBIDDEN")
            .await
            .expect("flag suspect");

        let addr = serve(state_router(
            StatusCode::NOT_FOUND,
            json!({ "errcode": "M_NOT_FOUND", "error": "unknown room" }),
        ))
        .await;
        reconcile_upstream_rooms(
            &client_for(addr).await,
            &store,
            account_id,
            &CancellationToken::new(),
        )
        .await;

        assert!(
            store
                .rooms_gone_upstream(account_id)
                .await
                .expect("gone")
                .contains(&room_id),
            "a 404 proves the homeserver does not serve the room"
        );
    }

    /// A served room clears its suspicion — the recovery path ADR 0090 promises.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn a_served_room_clears_its_suspicion() {
        let store = test_store().await;
        let account = store
            .upsert_account(
                &format!("@probe-200-{}:localhost", Uuid::new_v4()),
                "https://hs.example.org",
            )
            .await
            .expect("account");
        let account_id = account.account_id;
        let room_id = format!("!alive-{}:localhost", Uuid::new_v4());

        store
            .flag_room_upstream_suspect(account_id, &room_id, "M_FORBIDDEN")
            .await
            .expect("flag suspect");

        let addr = serve(state_router(
            StatusCode::OK,
            json!({ "room_version": "10", "creator": "@alice:localhost" }),
        ))
        .await;
        reconcile_upstream_rooms(
            &client_for(addr).await,
            &store,
            account_id,
            &CancellationToken::new(),
        )
        .await;

        assert!(
            store
                .suspect_upstream_rooms(account_id)
                .await
                .expect("suspects")
                .is_empty(),
            "a room that answers is no longer suspect"
        );
        assert!(
            store
                .rooms_gone_upstream(account_id)
                .await
                .expect("gone")
                .is_empty(),
            "and is certainly not gone"
        );
    }
}
