//! Runtime account lifecycle: the verbs that add, reactivate, and (later) stop
//! or remove a Matrix account while axon is running.
//!
//! [`AccountLifecycle`] is the concrete capability `axon-server` adapts onto the
//! API layer's `AccountLifecycle` port (mirroring how [`SdkGateway`](crate::gateway)
//! backs `MessageSender`) — `axon-api` never sees this type or any SDK type. It
//! owns the *lifecycle* state transitions (ADR 0022); connection mechanics live in
//! the [`ClientManager`], task supervision in the [`engine`](crate::engine).
//!
//! Concurrency: lifecycle verbs for one account must not interleave (a login
//! racing a future logout could strand a half-built session), so each verb runs
//! under a per-identity async lock keyed by Matrix `user_id` — the identity
//! login starts from, before any `account_id` exists. Homeserver URLs are
//! connection endpoints and may have multiple valid spellings.

use std::collections::HashMap;
use std::future::IntoFuture;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axon_core::{LiveFrame, SyncConfig};
use axon_media::MediaCacheHandle;
use axon_search::IndexHandle;
use axon_store::{Account, AccountState, CommitMatrixOAuthAcquire, Store, StoreError};
use matrix_sdk::authentication::oauth::OAuthSession;
use matrix_sdk::encryption::recovery::RecoveryError;
use matrix_sdk::encryption::secret_storage::{SecretStorageError, SecretStore};
use matrix_sdk::ruma::OwnedUserId;
use matrix_sdk::Client;
use tokio::sync::broadcast;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::backfill::BackfillHealth;
use crate::backup::{
    current_backup_signed_by_this_device, delete_backup_version, four_s_has_megolm,
    log_post_create_version, plan_backup, BackupAction, BackupPlan, BackupProbe, BackupVerb,
    ENABLE_EXPORT_TIMEOUT, UPLOAD_WAIT_TIMEOUT,
};
use crate::engine::{spawn_supervised, AccountTask, TaskRegistry};
use crate::error::{GatewayError, SyncError};
use crate::manager::ClientManager;
use crate::redecrypt::{RedecryptSummary, SweepScope};
use crate::sync_health::SyncHealth;
use crate::verification::{FlowRegistry, VerificationRooms};

/// How long logout waits for a cancelled supervised task to finish draining
/// (sync-service stop + re-decryption join) before escalating to an abort
/// (see [`AccountLifecycle::reap_task`]). Generous — a healthy drain is
/// sub-second — so hitting it means the task is wedged.
#[cfg(not(test))]
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for an aborted task to actually terminate. An abort lands
/// at the task's next await point, so this expires only for a task stuck in
/// non-yielding code — the one case reaping can fail.
#[cfg(not(test))]
const ABORT_TIMEOUT: Duration = Duration::from_secs(5);

// Test builds shrink the reap timeouts so the escalation paths (cancel-ignoring
// task → abort; unabortable task → Draining refusal) run in milliseconds.
#[cfg(test)]
const DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
#[cfg(test)]
const ABORT_TIMEOUT: Duration = Duration::from_millis(250);

/// Cap on the best-effort upstream `/logout` call, so the endpoint's response
/// time never depends on a stalled homeserver (the row is already deactivated
/// by the time this request is made).
const UPSTREAM_LOGOUT_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on `recover`'s under-lock UTD back-fill sweep (ADR 0026). The sweep loads
/// every pending UTD and pulls backup keys per room, so a large backlog or a
/// stalled homeserver could otherwise let `recover` hold the per-identity lock —
/// blocking a concurrent logout/delete — for an unbounded time. On timeout the
/// keys and `verified` are already persisted (the success the caller awaits); the
/// operator can retry the remaining rows explicitly or enable every-startup
/// retries in config.
const RECOVER_SWEEP_TIMEOUT: Duration = Duration::from_secs(30);
const MANUAL_REDECRYPT_TIMEOUT: Duration = Duration::from_secs(30);
const QR_TRUST_DERIVATION_TIMEOUT: Duration = Duration::from_secs(10);

/// What can go wrong running a lifecycle verb. Wire-neutral, like
/// [`GatewayError`](crate::GatewayError): the composition-root adapter
/// (`axon-server`) maps these onto the API layer's own login error so `axon-api`
/// never depends on this crate. Variants map cleanly to HTTP status: bad MXID →
/// 400, rejected credentials → 401, an account mid-teardown → 409, an
/// upstream/homeserver failure → 502, a store failure → 500.
#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    /// `username` is not a usable Matrix user ID: either syntactically invalid,
    /// or its domain is the homeserver's hostname rather than the server name
    /// its user IDs use (the message then suggests the canonical spelling).
    #[error("invalid matrix user id: {0}")]
    InvalidUserId(String),

    /// The account for this identity is mid-teardown (a transient `deleting`
    /// state), so a login can't be processed against it. Carries the account's id.
    /// (An *active* account is not an error — login is an idempotent no-op there.)
    #[error("account is being deleted: {0}")]
    BeingDeleted(Uuid),

    /// A QR acquisition completed remotely but another login made the identity
    /// active before finalization obtained its lifecycle lock. → 409.
    #[error("account is already active: {0}")]
    AlreadyActive(Uuid),

    /// A prior QR acquisition has crossed its durable commit point and still
    /// owns this identity while SDK-store adoption is reconciled. → 409.
    #[error("Matrix OAuth QR login is still finalizing for {0}")]
    LoginFinalizing(String),

    /// The QR protocol completed but the SDK's own device is not actually
    /// cross-signed, so Axon refuses to claim the combined login-and-verify
    /// outcome. → failed flow, never an active account.
    #[error("Matrix OAuth QR login did not produce a verified device")]
    DeviceNotVerified,

    /// No account exists for the given id. Raised by the id-keyed verbs
    /// (logout/delete); login never returns it (it mints a row for a new
    /// identity). Carries the id that was looked up. → 404.
    #[error("no such account: {0}")]
    NotFound(Uuid),

    /// The account's previous supervised task has not terminated — it survived
    /// both cancellation and an abort (wedged in non-yielding code), so its SDK
    /// store dir cannot be treated as quiescent. Verbs that would touch or
    /// restage that dir are refused until a retry reaps the task. → 409.
    #[error("sync task for account {0} is still draining; retry shortly")]
    Draining(Uuid),

    /// The account is not `active` (it is logged out / `deactivated`), so there
    /// is no live authenticated client to run the operation against. The caller
    /// must [`login`](AccountLifecycle::login) first to reactivate it. Raised by
    /// verbs that need a live session, e.g. [`recover`](AccountLifecycle::recover).
    /// → 409.
    #[error("account is not active: {0}")]
    NotActive(Uuid),

    /// Importing keys from the supplied recovery key failed — a wrong or rotated
    /// key, or an account whose Secure Backup was never set up. A readable client
    /// error, not a silent permanent UTD and not an internal failure. → 400.
    #[error("recovery failed: {0}")]
    RecoveryFailed(String),

    /// The operation cannot proceed because of the account's current backup
    /// state: unverified device, or a homeserver backup this device is not
    /// connected to and did not sign (ADR 0098 enable verb). → 409.
    #[error("{0}")]
    BackupConflict(String),

    /// The homeserver rejected the supplied credentials.
    #[error("authentication failed: {0}")]
    AuthFailed(String),

    /// The login could not be completed for a transient reason (homeserver
    /// unreachable, a 5xx, a malformed response).
    #[error("upstream homeserver error: {0}")]
    Upstream(String),

    /// A storage-layer failure while resolving or transitioning the account row.
    #[error("store error: {0}")]
    Store(String),
}

/// A Matrix OAuth acquisition finalization failure, including the staging-bound
/// client only while the durable session has not committed yet.
///
/// The protocol driver needs that client to revoke a freshly minted session on
/// a pre-commit failure. After commit, the breadcrumb owns recovery and the
/// client must already have been dropped before its SQLite directory moves.
pub(crate) struct MatrixOAuthAcquireFinalizeFailure {
    pub(crate) error: LifecycleError,
    pub(crate) acquisition_client: Option<Client>,
}

impl MatrixOAuthAcquireFinalizeFailure {
    fn before_commit(error: impl Into<LifecycleError>, acquisition_client: Client) -> Self {
        Self {
            error: error.into(),
            acquisition_client: Some(acquisition_client),
        }
    }

    fn after_commit(error: impl Into<LifecycleError>) -> Self {
        Self {
            error: error.into(),
            acquisition_client: None,
        }
    }
}

impl From<StoreError> for LifecycleError {
    fn from(err: StoreError) -> Self {
        LifecycleError::Store(err.to_string())
    }
}

impl From<SyncError> for LifecycleError {
    /// Map a login failure onto the lifecycle error: a rejected credential stays
    /// an auth failure (→ 401); a store failure stays a store error; everything
    /// else (connection, SDK build, bad response) is an upstream failure (→ 502).
    fn from(err: SyncError) -> Self {
        match err {
            SyncError::AuthFailed(msg) => LifecycleError::AuthFailed(msg),
            SyncError::Store(e) => LifecycleError::Store(e.to_string()),
            other => LifecycleError::Upstream(other.to_string()),
        }
    }
}

/// Re-derive whether axon's own device is currently cross-signed — the value
/// behind [`Account::verified`] (ADR 0026). Reads the SDK's current state
/// directly (`get_own_device().is_cross_signed_by_owner()`), the same check the
/// SDK uses internally, rather than the `verification_state()` subscriber: that
/// subscriber only refreshes on a `/keys/query` round-trip, so it lags an
/// immediate post-`recover` read. A missing own-device or a crypto-store error
/// is treated as **not** verified — the safe default, since a stale `true` is
/// worse than a transient `false`.
pub(crate) async fn derive_verified(client: &Client) -> bool {
    match client.encryption().get_own_device().await {
        Ok(Some(device)) => device.is_cross_signed_by_owner(),
        Ok(None) => false,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "failed to read own device for verification state; treating as unverified"
            );
            false
        }
    }
}

/// Re-derive and persist `verified` **under the per-identity lock** (ADR 0026).
/// This is the shared critical section the verification watcher uses: holding the
/// lock across the *derive* (not just the write) is what makes the observation
/// reflect any concurrent `recover`/`logout` write, so two derivers can't lose an
/// update (the watcher reading stale `false` and clobbering a recover's `true`).
/// Best-effort: a persist failure is logged, never fatal to the caller.
///
/// **Cancellation-aware acquisition (load-bearing).** The lock wait races `cancel`,
/// and a fired token wins (`biased`) and abandons the write. This is what keeps the
/// watcher from wedging shutdown: a lifecycle verb (logout/delete) holds this *same*
/// lock while it cancels and **awaits** the supervised task — which in turn awaits
/// this watcher. Were the lock wait un-cancellable, the watcher would park here
/// holding nothing while the verb waits on it holding the lock → deadlock until the
/// drain timeout aborts the supervisor, after which this separately-spawned task
/// could still acquire the freed lock and persist the dead device's value over the
/// verb's reset `false`. Bailing on `cancel` closes both (ADR 0026).
pub(crate) async fn lock_and_persist_verified(
    lock: &IdentityLock,
    client: &Client,
    store: &Store,
    account_id: Uuid,
    cancel: &CancellationToken,
) {
    let _guard = tokio::select! {
        biased;
        _ = cancel.cancelled() => return,
        guard = lock.lock() => guard,
    };
    let verified = derive_verified(client).await;
    if let Err(err) = store.set_account_verified(account_id, verified).await {
        tracing::warn!(
            %account_id,
            error = %err,
            "failed to persist device verification state"
        );
    }
}

/// Classify an SDK [`RecoveryError`] into a [`LifecycleError`] (ADR 0026). Only
/// the specific *key / backup-configuration* secret-storage variants are the
/// client's to act on — an undecodable recovery key, a wrong key (decryption
/// failure), an account with no secret storage, or a missing/inconsistent backup
/// key — so they become a readable `400` (`RecoveryFailed`) with a **stable**
/// message (the SDK's own text can leak secret-storage internals). Every other
/// variant — a *nested* SDK/network, JSON, crypto-store, secret-import, or
/// manual-verification failure, or any non-secret-storage `RecoveryError` — is
/// internal/upstream and not the caller's fault, so it maps to `Upstream`
/// (→ a logged, generic `500`) rather than being mis-blamed as a `400`.
fn classify_recovery_error(err: RecoveryError) -> LifecycleError {
    use matrix_sdk::encryption::secret_storage::SecretStorageError as Ss;

    let recovery_failed = || {
        LifecycleError::RecoveryFailed(
            "the recovery key was rejected, or the account has no usable Secure Backup".to_owned(),
        )
    };
    match err {
        RecoveryError::SecretStorage(ss) => match ss {
            // Client-actionable: the supplied key, or the account's backup config.
            Ss::SecretStorageKey(_)                 // recovery key won't decode/verify
            | Ss::MissingKeyInfo { .. }             // no secret storage configured
            | Ss::Decryption(_)                     // wrong key (decryption/MAC failure)
            | Ss::InconsistentBackupDecryptionKey
            | Ss::MissingOrInvalidBackupDecryptionKey => recovery_failed(),
            // Internal/upstream: nested SDK/network, JSON, crypto-store, import, or
            // verification failure. Keep the detail server-side, return a 500.
            other => LifecycleError::Upstream(other.to_string()),
        },
        // Enable-verb only: a backup already exists on the homeserver.
        // Recover never maps this join path to 409 (ADR 0098).
        RecoveryError::BackupExistsOnServer => LifecycleError::BackupConflict(
            "a megolm backup already exists on the homeserver; recover first to join it".to_owned(),
        ),
        // `Sdk` (upstream/SDK) is not key-actionable either.
        other => LifecycleError::Upstream(other.to_string()),
    }
}

/// 4S import connected cross-signing but the megolm backup key is missing or
/// does not match the homeserver version. Recover continues to the backup
/// tree (arm 3) instead of 400.
fn is_inconsistent_backup_key(err: &SecretStorageError) -> bool {
    use matrix_sdk::encryption::secret_storage::SecretStorageError as Ss;
    matches!(
        err,
        Ss::InconsistentBackupDecryptionKey | Ss::MissingOrInvalidBackupDecryptionKey
    )
}

/// Opening 4S for enable: no default key means we would have to mint one.
fn classify_open_store_for_enable(err: SecretStorageError) -> LifecycleError {
    use matrix_sdk::encryption::secret_storage::SecretStorageError as Ss;
    match err {
        Ss::MissingKeyInfo { .. } => LifecycleError::RecoveryFailed(
            "the account has no Secure Backup (4S); Axon will not mint a new recovery key"
                .to_owned(),
        ),
        other => classify_recovery_error(other.into()),
    }
}

/// The per-identity async mutex that serializes the lifecycle verbs (and the
/// verification watcher's `verified` write) for one Matrix user id.
pub(crate) type IdentityLock = Arc<AsyncMutex<()>>;

/// `canonical-identity → lock`. Owned by [`SyncEngine`](crate::SyncEngine) and
/// shared by every [`AccountLifecycle`] it hands out *and* by the supervised
/// tasks' verification watchers, so a verb and a watcher for the same identity
/// take the *same* lock (ADR 0026 — closes the `verified` lost-update race).
pub(crate) type IdentityLocks = Arc<Mutex<HashMap<String, IdentityLock>>>;

/// Fetch (or create) the per-identity lock for a Matrix user id. Homeserver base
/// URLs are connection endpoints, not distinct Matrix identities; config and
/// discovery may legitimately produce different URLs for the same user.
///
/// The std mutex is held only to fetch/insert; callers `await` the returned
/// async mutex, so verbs/watchers for *different* identities never block each
/// other.
///
/// The map grows unbounded — one entry per identity ever seen, never removed.
/// Pruning belongs to delete (which retires the identity for good), not logout: a
/// logged-out identity can be logged back in, and removing its lock while a verb
/// still holds it would let a concurrent login mint a fresh lock and run without
/// mutual exclusion. The leak is one small entry per distinct identity.
pub(crate) fn lock_for(
    locks: &IdentityLocks,
    user_id: &str,
    _homeserver_url: &str,
) -> IdentityLock {
    let key = user_id.to_owned();
    let mut map = locks.lock().expect("lifecycle lock map poisoned");
    map.entry(key).or_default().clone()
}

/// Runtime account-lifecycle capability. Cheap to [`Clone`] — every field is a
/// handle — so the adapter can hold one and call it per request. Shares the sync
/// engine's task tracker, cancellation token, and live-event bus, so an account
/// logged in here is supervised and shut down exactly like a boot-time one.
#[derive(Clone)]
pub struct AccountLifecycle {
    store: Store,
    config: SyncConfig,
    manager: ClientManager,
    live_tx: broadcast::Sender<LiveFrame>,
    cancel: CancellationToken,
    tracker: TaskTracker,
    /// Per-account task cancellation handles, shared with the engine. Logout
    /// cancels (and removes) the entry for the account it stops.
    tasks: TaskRegistry,
    /// `canonical-identity → lock`, shared with the engine and the supervised
    /// tasks' verification watchers (see [`IdentityLocks`]).
    locks: IdentityLocks,
    /// Verification-flow registry, shared with the engine so an account logged in
    /// here gets a supervised task whose incoming-request listener registers onto
    /// the same map the verification port reads.
    verifications: FlowRegistry,
    /// Per-account verification room subscriptions, shared with the engine so an
    /// account logged in here gets a supervised task whose sync loop subscribes the
    /// rooms cross-user verification flows run over (ADR 0040).
    verification_rooms: VerificationRooms,
    /// HTTP client for homeserver discovery (see [`discovery`](crate::discovery)).
    /// Cheap to clone (an `Arc` internally), shared across logins.
    http: matrix_sdk::reqwest::Client,
    /// Search-index producer (M9), or `None` when search is disabled. Used to purge
    /// a deleted account's documents, and handed to a runtime-login account's
    /// supervised task so its events are indexed.
    index: Option<IndexHandle>,
    /// Handle to the bounded media cache (M11). Used to purge a deleted account's
    /// cached media at teardown (ADR 0024 step 5).
    media: MediaCacheHandle,
    /// Backfill disk-space health (M10), shared with the engine so a runtime-login
    /// account's backfill task reports into the same handle the API reads.
    backfill_health: BackfillHealth,
    /// Per-account sync-service state, shared with the engine so a runtime-login
    /// account's supervised task reports into the same handle the API reads, and
    /// so logout/delete can clear the entry via [`sever_session`](Self::sever_session).
    sync_health: SyncHealth,
}

/// Outcome of [`AccountLifecycle::resolve_login_target`], the identity
/// resolution shared by `login` and `import_token`.
enum ResolvedTarget {
    /// An `active` row already satisfies the request; the caller returns this
    /// id unchanged.
    AlreadyActive(Uuid),
    /// A `deactivated` row to reuse (reactivate as a fresh device).
    Retained(Account),
    /// No existing row for this identity; the caller mints one.
    New,
}

impl AccountLifecycle {
    /// Build the lifecycle port. Called by [`SyncEngine::lifecycle`](crate::SyncEngine::lifecycle).
    // Each handle is a distinct shared resource the verbs need; bundling them into a
    // struct would just move the list, not shorten it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        store: Store,
        config: SyncConfig,
        manager: ClientManager,
        live_tx: broadcast::Sender<LiveFrame>,
        cancel: CancellationToken,
        tracker: TaskTracker,
        tasks: TaskRegistry,
        locks: IdentityLocks,
        verifications: FlowRegistry,
        verification_rooms: VerificationRooms,
        index: Option<IndexHandle>,
        media: MediaCacheHandle,
        backfill_health: BackfillHealth,
        sync_health: SyncHealth,
    ) -> Self {
        Self {
            store,
            config,
            manager,
            live_tx,
            cancel,
            tracker,
            tasks,
            locks,
            verifications,
            verification_rooms,
            http: crate::discovery::http_client(),
            index,
            media,
            backfill_health,
            sync_health,
        }
    }

    /// The per-identity lock for a Matrix user id, created on first use.
    /// Shared with the verification watcher via the engine-owned [`IdentityLocks`].
    fn lock_for(&self, user_id: &str, homeserver_url: &str) -> IdentityLock {
        lock_for(&self.locks, user_id, homeserver_url)
    }

    /// Cancel-aware UTD sweep: on timeout, cancel the token and await the
    /// partial summary so counts are not zeroed (ADR 0098).
    async fn sweep_pending_bounded(
        &self,
        client: &Client,
        account_id: Uuid,
        timeout: Duration,
    ) -> RedecryptSummary {
        let cancel = CancellationToken::new();
        let mut sweep = std::pin::pin!(crate::redecrypt::sweep_pending_utds(
            client,
            &self.store,
            account_id,
            SweepScope::AllPending,
            self.index.as_ref(),
            &cancel,
        ));
        tokio::select! {
            summary = &mut sweep => summary,
            _ = tokio::time::sleep(timeout) => {
                tracing::warn!(
                    %account_id,
                    timeout_secs = timeout.as_secs(),
                    "UTD back-fill sweep timed out; returning partial summary"
                );
                cancel.cancel();
                RedecryptSummary::timed_out(sweep.await)
            }
        }
    }

    /// Probe the SDK + 4S + intent, run the recover or enable tree, and
    /// perform enable/export/replace. Recover maps create/export failures to
    /// `BackupAction::Failed` at the caller; this method still returns Err
    /// for enable-verb 409/400 arms.
    async fn apply_backup_plan(
        &self,
        verb: BackupVerb,
        account: &Account,
        client: &Client,
        secret_store: Option<&SecretStore>,
        recovery_key_present: bool,
    ) -> Result<BackupAction, LifecycleError> {
        let backups = client.encryption().backups();
        let are_enabled = backups.are_enabled().await;
        let exists_on_server = match backups.fetch_exists_on_server().await {
            Ok(exists) => exists,
            Err(err) => {
                tracing::warn!(
                    account_id = %account.account_id,
                    error = %err,
                    "fetch_exists_on_server failed; treating as unknown"
                );
                if verb == BackupVerb::Recover {
                    return Ok(BackupAction::Failed);
                }
                return Err(LifecycleError::Upstream(err.to_string()));
            }
        };
        tracing::info!(
            account_id = %account.account_id,
            exists_on_server,
            are_enabled,
            "backup decision tree re-fetched exists_on_server"
        );

        let four_s_has_megolm = match secret_store {
            Some(store) => Some(four_s_has_megolm(store).await),
            None => None,
        };

        let mut signed_by_this_device = None;
        let mut current_version = None;
        if exists_on_server && !are_enabled {
            match current_backup_signed_by_this_device(client, account).await {
                Ok((version, signed, count)) => {
                    tracing::info!(
                        account_id = %account.account_id,
                        version = %version,
                        signed,
                        count,
                        "inspected current megolm backup auth_data"
                    );
                    signed_by_this_device = Some(signed);
                    current_version = Some(version);
                }
                Err(err) => {
                    tracing::warn!(
                        account_id = %account.account_id,
                        error = %err,
                        "failed to inspect current megolm backup version"
                    );
                }
            }
        }

        let probe = BackupProbe {
            verified: account.verified,
            recovery_key_present,
            are_enabled,
            exists_on_server,
            four_s_has_megolm,
            intent: account.backup_enable_intent,
            signed_by_this_device,
        };
        let plan = plan_backup(verb, &probe);
        match plan {
            BackupPlan::Unverified => Err(LifecycleError::BackupConflict(
                "account is not verified; recover or verify first".to_owned(),
            )),
            BackupPlan::NeedRecoveryKey => Err(LifecycleError::RecoveryFailed(
                "recovery_key is required to create or export megolm backup".to_owned(),
            )),
            BackupPlan::RefuseJoin => Err(LifecycleError::BackupConflict(
                "a megolm backup already exists on the homeserver; recover first to join it"
                    .to_owned(),
            )),
            BackupPlan::AlreadyUploading => Ok(BackupAction::AlreadyUploading),
            BackupPlan::Joined => Ok(BackupAction::Joined),
            BackupPlan::Failed => Ok(BackupAction::Failed),
            BackupPlan::ExportOnly => {
                let Some(store) = secret_store else {
                    return Err(LifecycleError::RecoveryFailed(
                        "recovery_key is required to create or export megolm backup".to_owned(),
                    ));
                };
                self.export_and_clear_intent(account.account_id, store)
                    .await
            }
            BackupPlan::EnableAndExport => {
                self.enable_and_export(account.account_id, client, secret_store, verb)
                    .await
            }
            BackupPlan::ReplaceThenEnable => {
                if let Some(version) = current_version {
                    if let Err(err) = delete_backup_version(client, version).await {
                        tracing::warn!(
                            account_id = %account.account_id,
                            error = %err,
                            "failed to delete our crashed megolm backup version"
                        );
                        return match verb {
                            BackupVerb::Recover => Ok(BackupAction::Failed),
                            BackupVerb::Enable => Err(err),
                        };
                    }
                }
                self.enable_and_export(account.account_id, client, secret_store, verb)
                    .await
            }
        }
    }

    async fn enable_and_export(
        &self,
        account_id: Uuid,
        client: &Client,
        secret_store: Option<&SecretStore>,
        verb: BackupVerb,
    ) -> Result<BackupAction, LifecycleError> {
        let Some(store) = secret_store else {
            return Err(LifecycleError::RecoveryFailed(
                "recovery_key is required to create or export megolm backup".to_owned(),
            ));
        };
        self.store
            .set_backup_enable_intent(account_id, true)
            .await?;
        let result = tokio::time::timeout(ENABLE_EXPORT_TIMEOUT, async {
            client
                .encryption()
                .recovery()
                .enable_backup()
                .await
                .map_err(classify_recovery_error)?;
            store
                .export_secrets()
                .await
                .map_err(|err| classify_recovery_error(err.into()))?;
            Ok::<(), LifecycleError>(())
        })
        .await;
        match result {
            Ok(Ok(())) => {
                self.store
                    .set_backup_enable_intent(account_id, false)
                    .await?;
                log_post_create_version(client, account_id).await;
                Ok(BackupAction::Enabled)
            }
            Ok(Err(LifecycleError::BackupConflict(_))) if verb == BackupVerb::Recover => {
                Ok(BackupAction::Failed)
            }
            Ok(Err(err)) => {
                if client.encryption().backups().are_enabled().await {
                    Ok(BackupAction::ExportPending)
                } else if verb == BackupVerb::Recover {
                    tracing::warn!(
                        %account_id,
                        error = %err,
                        "enable_backup failed after 4S import"
                    );
                    Ok(BackupAction::Failed)
                } else {
                    Err(err)
                }
            }
            Err(_) => {
                tracing::warn!(
                    %account_id,
                    timeout_secs = ENABLE_EXPORT_TIMEOUT.as_secs(),
                    "enable_backup/export_secrets timed out"
                );
                if client.encryption().backups().are_enabled().await {
                    Ok(BackupAction::ExportPending)
                } else if verb == BackupVerb::Recover {
                    Ok(BackupAction::Failed)
                } else {
                    Err(LifecycleError::Upstream(
                        "enabling megolm backup timed out".to_owned(),
                    ))
                }
            }
        }
    }

    async fn export_and_clear_intent(
        &self,
        account_id: Uuid,
        store: &SecretStore,
    ) -> Result<BackupAction, LifecycleError> {
        match tokio::time::timeout(ENABLE_EXPORT_TIMEOUT, store.export_secrets()).await {
            Ok(Ok(())) => {
                self.store
                    .set_backup_enable_intent(account_id, false)
                    .await?;
                Ok(BackupAction::Enabled)
            }
            Ok(Err(err)) => {
                tracing::warn!(
                    %account_id,
                    "export_secrets failed after local backup enable"
                );
                let _ = err;
                Ok(BackupAction::ExportPending)
            }
            Err(_) => {
                tracing::warn!(
                    %account_id,
                    timeout_secs = ENABLE_EXPORT_TIMEOUT.as_secs(),
                    "export_secrets timed out"
                );
                Ok(BackupAction::ExportPending)
            }
        }
    }

    /// Resolve an account, take its per-identity lock, and get a connected
    /// client — the shared preamble for verbs that must serialize against
    /// login/logout/delete and refuse a non-`Active` account: `recover()` and
    /// `redecrypt_utds()`. The returned guard must be held for the verb's
    /// entire body so the account can't change state underneath it.
    async fn lock_active_account_client(
        &self,
        account_id: Uuid,
    ) -> Result<(Account, Client, tokio::sync::OwnedMutexGuard<()>), LifecycleError> {
        // Resolve identity to take the per-identity lock (keyed by
        // `(user_id, homeserver_url)`, the key space the other verbs use); a 404
        // is cheap and needs no lock.
        let account = self
            .store
            .get_account(account_id)
            .await?
            .ok_or(LifecycleError::NotFound(account_id))?;

        let lock = self.lock_for(&account.user_id, &account.homeserver_url);
        let guard = lock.lock_owned().await;

        // Re-read under the lock: the state may have moved between the unlocked
        // resolve above and acquiring the lock. The lock serializes this against
        // login/logout/delete, so the state checked here can't change under us.
        let account = self
            .store
            .get_account(account_id)
            .await?
            .ok_or(LifecycleError::NotFound(account_id))?;
        match account.state {
            AccountState::Active => {}
            AccountState::Deactivated => return Err(LifecycleError::NotActive(account_id)),
            AccountState::Deleting => return Err(LifecycleError::BeingDeleted(account_id)),
        }

        // The supervised task normally holds a cached client; the cold-connect
        // gate (active-only) is satisfied by the check above. Map the gateway's
        // errors onto the lifecycle ones — under the lock on an `active` row the
        // not-active/unknown arms are unreachable, but stay defensive.
        let client = self
            .manager
            .get_or_connect(account_id)
            .await
            .map_err(|err| match err {
                GatewayError::UnknownAccount(id) => LifecycleError::NotFound(id),
                GatewayError::AccountNotActive(id) => LifecycleError::NotActive(id),
                other => LifecycleError::Upstream(other.to_string()),
            })?;

        Ok((account, client, guard))
    }

    /// Log a Matrix account in at runtime as a fresh device, returning its Axon
    /// `account_id`. Idempotent by Matrix user id:
    ///
    /// - **New identity** → mint a row.
    /// - **`deactivated` row** → reuse its `account_id` (and its retained Postgres
    ///   archive), logging in as a fresh device with a fresh SDK crypto store.
    /// - **`active` row** → **idempotent no-op**: return the existing `account_id`
    ///   unchanged. The account is already logged in and supervised, so we do *not*
    ///   re-log-in (which would wipe its store out from under the running task).
    /// - **`deleting` row** → [`LifecycleError::BeingDeleted`] (409): a row
    ///   mid-teardown can't be logged into.
    ///
    /// For a new/deactivated row the row is held `deactivated` until the homeserver
    /// login succeeds, so a failed login never leaves a dangling `active` account
    /// and never deletes the row. On success it flips to `active` and a supervised
    /// sync task is spawned. `username` must be a full MXID; the password is
    /// consumed once, never stored (and not consulted at all for the active no-op).
    ///
    /// `homeserver_url` is optional: when absent it is resolved from the MXID's
    /// server name (see [`discovery`](crate::discovery)), so the canonical URL —
    /// not whatever each client guessed — keys the identity. A failed discovery
    /// is an upstream error and touches nothing. On both paths the MXID's domain
    /// is then checked against the homeserver's own declared server name
    /// (best-effort): an MXID written with the homeserver's hostname
    /// (`@adam:matrix.example.org` for `@adam:example.org`) is rejected with a
    /// did-you-mean error naming the canonical spelling, rather than failing as
    /// a misleading auth error — or, worse, being logged in as an identity other
    /// than the one typed.
    pub async fn login(
        &self,
        homeserver_url: Option<&str>,
        username: &str,
        password: &str,
    ) -> Result<Uuid, LifecycleError> {
        // Validate the MXID up front so identity resolves before we touch the DB
        // or build an SDK store.
        let user_id = OwnedUserId::try_from(username)
            .map_err(|e| LifecycleError::InvalidUserId(format!("{username}: {e}")))?;

        // Serialize before discovery so concurrent requests using different
        // endpoint spellings cannot both observe a missing Matrix identity.
        let lock = self.lock_for(username, "");
        let _guard = lock.lock().await;

        self.ensure_no_matrix_oauth_acquire(username).await?;

        let account = match self.resolve_login_target(username).await? {
            ResolvedTarget::AlreadyActive(id) => return Ok(id),
            ResolvedTarget::Retained(existing) => Some(existing),
            ResolvedTarget::New => None,
        };

        // A retained account keeps using its stored endpoint. Only a genuinely
        // new Matrix identity needs caller normalization or server-side discovery.
        let homeserver_url = match account.as_ref() {
            Some(existing) => existing.homeserver_url.clone(),
            None => match homeserver_url {
                // Normalize + scheme-check the caller's URL (trailing slash
                // trimmed; plain-HTTP public hosts refused so the password can't
                // leave in cleartext). Not probed — a bad URL surfaces at login.
                Some(url) => {
                    crate::discovery::accept_explicit_homeserver(user_id.server_name(), url)
                        .map_err(|e| LifecycleError::Upstream(e.to_string()))?
                }
                None => crate::discovery::resolve_homeserver(&self.http, user_id.server_name())
                    .await
                    .map_err(|e| LifecycleError::Upstream(e.to_string()))?,
            },
        };
        let homeserver_url = homeserver_url.as_str();

        // Refuse an MXID whose domain is actually the homeserver's hostname —
        // no such user can exist there, so fail with the spelling they meant
        // instead of a misleading auth error. This probe is deliberately after
        // the active/deleting/draining short-circuits: an idempotent active
        // login stays a pure local no-op. It is still before any new row or SDK
        // store is created.
        crate::discovery::check_user_id_domain(&self.http, homeserver_url, &user_id)
            .await
            .map_err(|e| LifecycleError::InvalidUserId(e.to_string()))?;

        let account = match account {
            Some(existing) => existing,
            None => {
                self.mint_deactivated_account(username, homeserver_url)
                    .await?
            }
        };

        // Log in as a fresh device; the manager caches the live client in the
        // account's slot so the supervised task reuses it (ADR 0021). A failure
        // leaves the row `deactivated`.
        self.manager.login(&account, password).await?;

        let account_id = self.activate_and_supervise(account.account_id).await?;
        tracing::info!(%account_id, user_id = %username, "account logged in and supervised");
        Ok(account_id)
    }

    /// Resolve `username` against an existing account for a login-shaped verb
    /// (`login`, `import_token`) and apply their shared idempotency contract:
    ///
    /// - **`active` row** → [`AlreadyActive`](ResolvedTarget::AlreadyActive) —
    ///   the caller returns this id unchanged; the credential is not consulted.
    /// - **`deleting` row** → [`LifecycleError::BeingDeleted`] (409): a row
    ///   mid-teardown can't be logged into.
    /// - **`deactivated` row** → [`Retained`](ResolvedTarget::Retained), unless
    ///   a registered supervised task means reactivation would restage the SDK
    ///   store dir out from under it (only possible when a logout failed to
    ///   reap the task — `reap_task` re-registers a wedged one), in which case
    ///   [`LifecycleError::Draining`] (409): a logout retry reaps it.
    /// - **no such row** → [`New`](ResolvedTarget::New).
    async fn resolve_login_target(&self, username: &str) -> Result<ResolvedTarget, LifecycleError> {
        // Resolve by Matrix id, not homeserver URL. A configured endpoint and a
        // discovered client endpoint can differ while naming the same account;
        // treating the URL as identity used to mint duplicate active rows.
        match self.store.find_account_by_user_id(username).await? {
            Some(existing) => match existing.state {
                AccountState::Active => Ok(ResolvedTarget::AlreadyActive(existing.account_id)),
                AccountState::Deleting => Err(LifecycleError::BeingDeleted(existing.account_id)),
                AccountState::Deactivated => {
                    if self
                        .tasks
                        .lock()
                        .expect("task registry poisoned")
                        .contains_key(&existing.account_id)
                    {
                        Err(LifecycleError::Draining(existing.account_id))
                    } else {
                        Ok(ResolvedTarget::Retained(existing))
                    }
                }
            },
            None => Ok(ResolvedTarget::New),
        }
    }

    async fn ensure_no_matrix_oauth_acquire(&self, username: &str) -> Result<(), LifecycleError> {
        if self
            .store
            .has_matrix_oauth_acquire_for_user(username)
            .await?
        {
            Err(LifecycleError::LoginFinalizing(username.to_owned()))
        } else {
            Ok(())
        }
    }

    /// Reserve one Matrix identity for a QR acquisition under the same lock as
    /// password login, token import, deletion, and finalization. The lock covers
    /// only the local conflict decision and durable breadcrumb insertion; the
    /// multi-minute remote protocol starts after this method returns.
    pub(crate) async fn reserve_matrix_oauth_acquire(
        &self,
        flow_id: Uuid,
        expected_user_id: &str,
        presentation: &str,
        staging_dir_name: &str,
    ) -> Result<(), LifecycleError> {
        let lock = self.lock_for(expected_user_id, "");
        let _guard = lock.lock().await;

        match self.resolve_login_target(expected_user_id).await? {
            ResolvedTarget::AlreadyActive(account_id) => {
                return Err(LifecycleError::AlreadyActive(account_id))
            }
            ResolvedTarget::Retained(_) | ResolvedTarget::New => {}
        }
        if !self
            .store
            .create_matrix_oauth_acquire_breadcrumb(
                flow_id,
                expected_user_id,
                presentation,
                staging_dir_name,
            )
            .await?
        {
            return Err(LifecycleError::LoginFinalizing(expected_user_id.to_owned()));
        }
        Ok(())
    }

    /// Mint a fresh account row and hold it `deactivated` until the caller's
    /// credential is confirmed — shared by `login` and `import_token` for the
    /// no-existing-row case.
    ///
    /// NOTE: the insert and the state flip are not atomic. A crash between them
    /// leaves an orphaned `active` row with no stored session and no running
    /// task; the boot reconcile / orphan GC is what retires such rows (the
    /// credential can't be replayed to finish it).
    async fn mint_deactivated_account(
        &self,
        username: &str,
        homeserver_url: &str,
    ) -> Result<Account, LifecycleError> {
        let minted = self.store.upsert_account(username, homeserver_url).await?;
        self.store
            .set_account_state(minted.account_id, AccountState::Deactivated)
            .await?;
        Ok(minted)
    }

    /// Activate a freshly-authenticated account and spawn its supervised task —
    /// the shared tail of `login` and `import_token`, once the manager has
    /// already cached a live client in the account's slot (still `deactivated`
    /// at this point). If activation fails, evicts that cached client: no
    /// supervised task will consume it, and leaving it cached would let a later
    /// send reach a live client behind the active-state gate on an account that
    /// never became active.
    async fn activate_and_supervise(&self, account_id: Uuid) -> Result<Uuid, LifecycleError> {
        let active = match self.activate(account_id).await {
            Ok(active) => active,
            Err(err) => {
                self.manager.evict(account_id).await;
                return Err(err);
            }
        };
        Ok(self.supervise(active))
    }

    /// Register an already-active account on the engine's ordinary supervised
    /// path. Shared by password/token activation and QR-store adoption so the
    /// latter cannot drift into a second supervision implementation.
    fn supervise(&self, active: Account) -> Uuid {
        let account_id = active.account_id;
        spawn_supervised(
            &self.tracker,
            &self.tasks,
            self.store.clone(),
            self.config.clone(),
            active,
            self.cancel.clone(),
            self.live_tx.clone(),
            self.manager.clone(),
            self.locks.clone(),
            self.verifications.clone(),
            self.verification_rooms.clone(),
            self.index.clone(),
            self.backfill_health.clone(),
            self.sync_health.clone(),
        );
        account_id
    }

    /// Finalize a successful Matrix OAuth QR login under the same per-identity
    /// lock as login/logout/delete. The encrypted session and durable adoption
    /// breadcrumb commit together while the account remains deactivated; only
    /// after the SDK store is in its permanent location is the account activated
    /// and handed to the normal supervisor.
    pub(crate) async fn finalize_matrix_oauth_acquire(
        &self,
        flow_id: Uuid,
        staging_dir_name: &str,
        expected_user_id: &str,
        client: Client,
        session: OAuthSession,
    ) -> Result<Uuid, MatrixOAuthAcquireFinalizeFailure> {
        let lock = self.lock_for(expected_user_id, "");
        let _guard = lock.lock().await;

        // Keep all fallible pre-commit work in one borrowing future. If it
        // fails, ownership of `client` remains here so the protocol driver can
        // revoke the minted session. Once this returns an account, the durable
        // breadcrumb owns recovery and the client can be consumed by promotion.
        let precommit: Result<Account, LifecycleError> = async {
            match self.resolve_login_target(expected_user_id).await? {
                ResolvedTarget::AlreadyActive(account_id) => {
                    return Err(LifecycleError::AlreadyActive(account_id))
                }
                ResolvedTarget::Retained(_) | ResolvedTarget::New => {}
            }

            if !tokio::time::timeout(QR_TRUST_DERIVATION_TIMEOUT, derive_verified(&client))
                .await
                .unwrap_or(false)
            {
                return Err(LifecycleError::DeviceNotVerified);
            }

            let refresh_token = session
                .user
                .tokens
                .refresh_token
                .as_deref()
                .ok_or_else(|| {
                    LifecycleError::Upstream(
                        "Matrix OAuth QR login returned no refresh token".to_owned(),
                    )
                })?;
            if session.user.meta.user_id.as_str() != expected_user_id {
                return Err(LifecycleError::AuthFailed(
                    "Matrix OAuth QR login returned a different Matrix user".to_owned(),
                ));
            }
            let store_key = self
                .config
                .store_key
                .as_deref()
                .ok_or(SyncError::MissingStoreKey)?;
            let homeserver_url = client.homeserver().to_string();
            match self
                .store
                .commit_matrix_oauth_acquire(
                    flow_id,
                    expected_user_id,
                    &homeserver_url,
                    session.user.meta.device_id.as_str(),
                    &session.user.tokens.access_token,
                    refresh_token,
                    session.client_id.as_str(),
                    store_key,
                )
                .await?
            {
                CommitMatrixOAuthAcquire::Committed(account) => Ok(account),
                CommitMatrixOAuthAcquire::ActiveConflict(account_id) => {
                    Err(LifecycleError::AlreadyActive(account_id))
                }
                CommitMatrixOAuthAcquire::DeletingConflict(account_id) => {
                    Err(LifecycleError::BeingDeleted(account_id))
                }
            }
        }
        .await;
        let account = match precommit {
            Ok(account) => account,
            Err(error) => {
                return Err(MatrixOAuthAcquireFinalizeFailure::before_commit(
                    error, client,
                ))
            }
        };

        if let Err(error) = self
            .manager
            .promote_matrix_oauth_acquire(&account, staging_dir_name, client)
            .await
        {
            return Err(MatrixOAuthAcquireFinalizeFailure::after_commit(error));
        }
        let finalized = self
            .store
            .finalize_matrix_oauth_acquire(flow_id, account.account_id)
            .await
            .map_err(MatrixOAuthAcquireFinalizeFailure::after_commit)?;
        if !finalized {
            self.manager.evict(account.account_id).await;
            return Err(MatrixOAuthAcquireFinalizeFailure::after_commit(
                LifecycleError::Store(
                    "Matrix OAuth QR finalization breadcrumb disappeared".to_owned(),
                ),
            ));
        }
        // The activation transaction succeeded, so use the row we already own
        // rather than introducing a fallible read between activation and
        // supervision. No account fields other than lifecycle/trust change in
        // that transaction.
        let mut active = account;
        active.state = AccountState::Active;
        active.verified = true;
        let account_id = self.supervise(active);
        tracing::info!(
            %flow_id,
            %account_id,
            user_id = expected_user_id,
            "Matrix OAuth QR login finalized and supervised"
        );
        Ok(account_id)
    }

    /// Adopt an existing Matrix `access_token` + `device_id` as a runtime
    /// account, returning its Axon `account_id` — the runtime replacement for
    /// the retired `sync.account.access_token` boot-time pre-provisioning path
    /// (GH #65/#66): restore a session axon didn't mint itself (e.g. one issued by
    /// another client, or an SSO-only account with no password) without a fresh
    /// login. Idempotent by Matrix user id, exactly like [`login`](Self::login):
    ///
    /// - **New identity** → mint a row, using the caller's `homeserver_url`.
    /// - **`deactivated` row** → reuse its `account_id` (and its retained
    ///   Postgres archive), importing the token as a fresh device with a fresh
    ///   SDK crypto store — the account's *stored* `homeserver_url` is used,
    ///   never the caller's.
    /// - **`active` row** → **idempotent no-op**: return the existing
    ///   `account_id` unchanged; the token is not consulted.
    /// - **`deleting` row** → [`LifecycleError::BeingDeleted`] (409).
    ///
    /// Unlike `login`, no homeserver call confirms the token before this runs —
    /// session restore is a purely local SDK operation — so the underlying
    /// connect validates it with a `whoami` round-trip: a token that doesn't
    /// belong to `username` (or, if the homeserver reports one, `device_id`) is
    /// rejected as [`LifecycleError::AuthFailed`], same as an unknown/revoked
    /// token. `homeserver_url` is required (there is no MXID to discover it
    /// from, unlike `login`'s optional parameter) and is consulted only when
    /// minting a new row.
    pub async fn import_token(
        &self,
        homeserver_url: &str,
        username: &str,
        access_token: &str,
        device_id: &str,
    ) -> Result<Uuid, LifecycleError> {
        OwnedUserId::try_from(username)
            .map_err(|e| LifecycleError::InvalidUserId(format!("{username}: {e}")))?;

        // Serialize before any store/homeserver work, same key space `login` uses.
        let lock = self.lock_for(username, "");
        let _guard = lock.lock().await;

        self.ensure_no_matrix_oauth_acquire(username).await?;

        let account = match self.resolve_login_target(username).await? {
            ResolvedTarget::AlreadyActive(id) => return Ok(id),
            ResolvedTarget::Retained(existing) => existing,
            ResolvedTarget::New => {
                // Hold `deactivated` until the token is confirmed valid below, so a
                // rejected/mismatched token leaves no live account (mirrors `login`).
                self.mint_deactivated_account(username, homeserver_url)
                    .await?
            }
        };

        self.manager
            .import_token(&account, access_token, device_id)
            .await?;

        let account_id = self.activate_and_supervise(account.account_id).await?;
        tracing::info!(%account_id, user_id = %username, "account token imported and supervised");
        Ok(account_id)
    }

    /// Flip a freshly-logged-in account to `active` and re-read the row. Split out
    /// of [`login`](Self::login) so the caller can evict the login's cached client
    /// if any step here fails — otherwise a failed activation would strand a usable
    /// client on a non-`active` account.
    async fn activate(&self, account_id: Uuid) -> Result<Account, LifecycleError> {
        self.store
            .set_account_state(account_id, AccountState::Active)
            .await?;
        self.store.get_account(account_id).await?.ok_or_else(|| {
            LifecycleError::Store(format!("account {account_id} vanished after login"))
        })
    }

    /// Log a Matrix account out at runtime: move the row to `deactivated`, stop
    /// its supervised sync task **and await its drain**, then invalidate its
    /// device token upstream (best-effort, capped). All of axon's data is
    /// **retained** (the Postgres archive and the on-disk SDK store), so a later
    /// [`login`](Self::login) reactivates the same `account_id` as a fresh
    /// device. On `Ok` the account's task has *terminated* and its store dir is
    /// quiescent, so an immediate re-login is safe; if the task cannot be made
    /// to terminate (survives cancel **and** abort — see
    /// [`reap_task`](Self::reap_task)) this fails with
    /// [`LifecycleError::Draining`] instead, the task stays registered, and
    /// [`login`](Self::login) refuses the identity until a logout retry reaps
    /// it — the postcondition is never traded away for a return. Keyed by
    /// `account_id`:
    ///
    /// - **`active` row** → stop + deactivate.
    /// - **`deactivated` row** → idempotent re-run of the severing (a no-op when
    ///   the row was cleanly logged out; finishes the job after a logout that
    ///   failed midway).
    /// - **`deleting` row** → [`LifecycleError::BeingDeleted`] (409): a delete is in
    ///   flight; don't interfere.
    /// - **no such row** → [`LifecycleError::NotFound`] (404).
    pub async fn logout(&self, account_id: Uuid) -> Result<(), LifecycleError> {
        // Resolve identity so we can take the per-identity lock (keyed by
        // `(user_id, homeserver_url)`, the key space login uses). A 404 is cheap
        // and needs no lock.
        let account = self
            .store
            .get_account(account_id)
            .await?
            .ok_or(LifecycleError::NotFound(account_id))?;

        let lock = self.lock_for(&account.user_id, &account.homeserver_url);
        let _guard = lock.lock().await;

        // Re-read under the lock: the state may have moved between the unlocked
        // resolve above and acquiring the lock.
        let account = self
            .store
            .get_account(account_id)
            .await?
            .ok_or(LifecycleError::NotFound(account_id))?;
        match account.state {
            // Mid-teardown: a delete is in flight (409).
            AccountState::Deleting => return Err(LifecycleError::BeingDeleted(account_id)),
            // Deactivate FIRST. `get_or_connect`'s cold-connect gate refuses a
            // non-`active` row, so once this write lands no *new* client can be
            // built for the account — without it, a send racing the steps below
            // could cold-connect into the just-emptied slot while the row still
            // reads `active`, and the cached client it leaves behind would
            // outlive the deactivation (the gate doesn't re-check state on a
            // cache hit).
            AccountState::Active => {
                self.store
                    .set_account_state(account_id, AccountState::Deactivated)
                    .await?;
            }
            // Already logged out — but fall through to the severing below rather
            // than returning. On a cleanly logged-out row it's all no-ops; after
            // a logout that failed midway (a wedged task, a 500 between the state
            // flip and the eviction) it's what lets a retry finish the job.
            AccountState::Deactivated => {}
        }

        // Clear `verified` immediately after the state transition and BEFORE the
        // fallible sever (ADR 0026). The device is logged out — its token is dead and
        // a re-login mints a *fresh, unverified* device — so a stale `true` here
        // would be returned by the next login's read-back or a by-id read of the
        // `deactivated` row, which the spec forbids ("a stale `true` is worse than
        // `false`"). This MUST precede `sever_session`: that call returns `Draining`
        // when a wedged task survives cancel+abort, leaving the row `deactivated` —
        // so a clear placed after it would be skipped exactly when the row stays
        // client-visible. Safe under the identity lock we hold: the verification
        // watcher takes the same lock for its own write, so it can't clobber this
        // (and on the retry/already-`deactivated` path it re-clears harmlessly).
        self.store.set_account_verified(account_id, false).await?;

        // Sever the live session: reap the supervised task (awaiting its drain)
        // and take + upstream-invalidate the cached client. Shared with `delete`.
        // A wedged task surfaces as `Draining` and leaves the row `deactivated` for
        // a retry to reap.
        self.sever_session(account_id).await?;

        tracing::info!(%account_id, user_id = %account.user_id, "account logged out");
        Ok(())
    }

    /// Acquire E2EE keys for an account from its Secure-Storage (4S) **recovery
    /// key** and self-verify axon's device — the bootstrap/fallback
    /// key-acquisition path (ADR 0011), originally promoted from a boot-time
    /// config call to this on-demand verb; the boot path it was promoted from
    /// has since been retired along with config-based provisioning (ADR 0024).
    /// A single SDK call (`recovery().recover`) imports both the megolm key
    /// **backup** decryption key and the cross-signing private keys into the
    /// account's crypto store, which (1) lets axon cross-sign its own device
    /// with no interactive partner and (2) unlocks already-stored UTDs — which
    /// we then back-fill through the re-decryption queue's sweep. On success
    /// the account's `verified` flag is re-derived from the SDK and persisted
    /// so a client reads the new state immediately (ADR 0026). The
    /// recovery-key string is consumed here and **never persisted**.
    ///
    /// Requires an **active** account (recover runs against its live,
    /// authenticated client). Keyed by `account_id`:
    /// - **`active` row** → recover.
    /// - **`deactivated` row** → [`LifecycleError::NotActive`] (409): log in first.
    /// - **`deleting` row** → [`LifecycleError::BeingDeleted`] (409).
    /// - **no such row** → [`LifecycleError::NotFound`] (404).
    ///
    /// A wrong/rotated key, or an account with no Secure Backup, is a readable
    /// [`LifecycleError::RecoveryFailed`] (400) — never a silent permanent UTD.
    /// A 200 means 4S import succeeded; `backup_action` and the sweep summary
    /// tell whether megolm backup was enabled/joined and whether the UTD sweep
    /// timed out with real counts (ADR 0098).
    pub async fn recover(
        &self,
        account_id: Uuid,
        recovery_key: &str,
    ) -> Result<(BackupAction, RedecryptSummary), LifecycleError> {
        // Resolve identity, take the per-identity lock, re-check state under it,
        // and get a connected client — the same preamble `redecrypt_utds` uses.
        let (account, client, _guard) = self.lock_active_account_client(account_id).await?;

        // One SecretStore for the whole verb: import, then maybe enable/export.
        // `Recovery::recover` would drop the store before we can export.
        let secret_store = client
            .encryption()
            .secret_storage()
            .open_secret_store(recovery_key)
            .await
            .map_err(|err| classify_recovery_error(err.into()))?;

        let verified = if let Err(err) = secret_store.import_secrets().await {
            let verified = derive_verified(&client).await;
            if !(verified && is_inconsistent_backup_key(&err)) {
                return Err(classify_recovery_error(err.into()));
            }
            tracing::info!(
                %account_id,
                "4S import left megolm backup key missing or inconsistent; continuing recover tree"
            );
            verified
        } else {
            derive_verified(&client).await
        };

        // Persist `verified` straight away so the row the caller reads back
        // reflects it. After a successful 4S import the cross-signing keys
        // just landed and this device is now self-cross-signed. The sync
        // watcher (ADR 0026) would also catch this on the next keys-query,
        // but recover's caller reads the row immediately, and
        // `verification_state()` lags that round-trip.
        self.store
            .set_account_verified(account_id, verified)
            .await?;
        let mut account = account;
        account.verified = verified;

        let backup_action = self
            .apply_backup_plan(
                BackupVerb::Recover,
                &account,
                &client,
                Some(&secret_store),
                true,
            )
            .await
            .unwrap_or_else(|err| {
                // 4S import already succeeded: recover stays 200 with failed.
                // Enable-verb 409 mapping must not leak onto this path.
                tracing::warn!(
                    %account_id,
                    error = %err,
                    "recover backup enable/export failed after 4S import"
                );
                BackupAction::Failed
            });

        // Back-fill any stored UTDs the imported keys now unlock. Cancel-aware:
        // on the 30s cap we cancel the token and await the partial summary so
        // recover's envelope does not report selected=0 (ADR 0098).
        let summary = self
            .sweep_pending_bounded(&client, account_id, RECOVER_SWEEP_TIMEOUT)
            .await;

        tracing::info!(
            %account_id,
            user_id = %account.user_id,
            verified,
            backup_action = ?backup_action,
            selected = summary.selected,
            attempted = summary.attempted,
            decrypted = summary.decrypted,
            still_pending = summary.still_pending,
            timed_out = summary.timed_out,
            "account recovered keys via recovery key"
        );
        Ok((backup_action, summary))
    }

    /// Originate, export-only resume, crash-resume replace, or kick upload for
    /// megolm key backup (ADR 0098). Drops the identity lock before the
    /// bounded `wait_for_steady_state`.
    pub async fn enable_backup(
        &self,
        account_id: Uuid,
        recovery_key: Option<&str>,
    ) -> Result<BackupAction, LifecycleError> {
        let (account, client, guard) = self.lock_active_account_client(account_id).await?;

        let secret_store = if let Some(key) = recovery_key {
            Some(
                client
                    .encryption()
                    .secret_storage()
                    .open_secret_store(key)
                    .await
                    .map_err(classify_open_store_for_enable)?,
            )
        } else {
            None
        };

        let action = self
            .apply_backup_plan(
                BackupVerb::Enable,
                &account,
                &client,
                secret_store.as_ref(),
                recovery_key.is_some(),
            )
            .await?;
        drop(guard);

        if matches!(
            action,
            BackupAction::Enabled | BackupAction::AlreadyUploading
        ) {
            let backups = client.encryption().backups();
            let wait = backups.wait_for_steady_state().into_future();
            match tokio::time::timeout(UPLOAD_WAIT_TIMEOUT, wait).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => tracing::warn!(
                    %account_id,
                    error = %err,
                    "backup upload wait ended before Done; SDK continues"
                ),
                Err(_) => tracing::info!(
                    %account_id,
                    timeout_secs = UPLOAD_WAIT_TIMEOUT.as_secs(),
                    "backup upload wait timed out; SDK continues uploading"
                ),
            }
        }

        tracing::info!(
            %account_id,
            user_id = %account.user_id,
            backup_action = ?action,
            verified = account.verified,
            backup_enable_intent = account.backup_enable_intent,
            "megolm backup enable verb completed"
        );
        Ok(action)
    }

    /// Explicitly retry every pending UTD for an active account. This is the
    /// operator escape hatch for the default startup policy, which attempts each
    /// UTD only once at boot and then waits for fresh room-key arrivals.
    pub async fn redecrypt_utds(
        &self,
        account_id: Uuid,
    ) -> Result<RedecryptSummary, LifecycleError> {
        let (_account, client, _guard) = self.lock_active_account_client(account_id).await?;

        let summary = self
            .sweep_pending_bounded(&client, account_id, MANUAL_REDECRYPT_TIMEOUT)
            .await;
        tracing::info!(
            %account_id,
            selected = summary.selected,
            attempted = summary.attempted,
            decrypted = summary.decrypted,
            still_pending = summary.still_pending,
            timed_out = summary.timed_out,
            "manual UTD re-decryption sweep completed"
        );
        Ok(summary)
    }

    /// Stop an account's live session: reap its supervised task **awaiting its
    /// drain** (cooperative cancel → abort escalation; a task that survives both
    /// is left registered and surfaces as [`LifecycleError::Draining`]), then take
    /// its cached client out of the connection slot and best-effort, time-capped,
    /// invalidate the device token upstream. Shared by [`logout`](Self::logout)
    /// and [`delete`](Self::delete) — the "sever the running session" tail both
    /// need.
    ///
    /// Preconditions (the caller's job): the row has already been flipped out of
    /// `active` (so the cold-connect gate refuses any *new* client — this is the
    /// flip-before-take that closes the reconnect race), and the per-identity lock
    /// is held. `take` awaits the slot lock, so a connect that read `active` before
    /// the flip and cached a client has it taken right back out here.
    ///
    /// Returns the reap result: on `Draining` the caller must **not** proceed to
    /// remove or restage anything the still-live task may hold — its store dir is
    /// not quiescent. The upstream call never fails the verb: an unreachable or
    /// stalled homeserver must not stall it (the local state is already changed),
    /// so the device merely lingers upstream until reachable.
    async fn sever_session(&self, account_id: Uuid) -> Result<(), LifecycleError> {
        self.reap_task(account_id).await?;

        if let Some(client) = self.manager.take(account_id).await {
            // `Client::logout` dispatches to the session's matching auth
            // implementation: Matrix `/logout` for legacy sessions, OAuth token
            // revocation for OAuth sessions. Local teardown is already complete,
            // so either upstream failure remains bounded and best-effort.
            match tokio::time::timeout(UPSTREAM_LOGOUT_TIMEOUT, client.logout()).await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => tracing::warn!(
                    %account_id,
                    error = %err,
                    "upstream logout failed; session severed locally"
                ),
                Err(_) => tracing::warn!(
                    %account_id,
                    timeout_secs = UPSTREAM_LOGOUT_TIMEOUT.as_secs(),
                    "upstream logout timed out; session severed locally"
                ),
            }
        }
        Ok(())
    }

    /// Permanently delete a Matrix account and every trace of it — an **ordered,
    /// idempotent, crash-recoverable** teardown (ADR 0024). Unlike
    /// [`logout`](Self::logout), which is a reversible pause that *retains* all
    /// data, this is a hard removal: the row, its Postgres archive (via FK
    /// cascade), and its on-disk SDK store are gone, and re-adding the same Matrix
    /// account later is a fresh [`login`](Self::login) with a new `account_id`.
    ///
    /// The order is load-bearing — the row is the only durable key a boot reconcile
    /// can re-find the external resources from, so it is deleted **last**:
    /// 1. flip the row to `deleting` (a durable "external cleanup owed" marker;
    ///    also moves it out of `active` so the cold-connect gate refuses any new
    ///    client *before* the cached one is taken — flip-before-take);
    /// 2. [`sever_session`](Self::sever_session) the live session;
    /// 3. remove the on-disk SDK store dir (and its staging backup);
    /// 4. purge the account's entries from the media cache (M11, ADR 0024 step 5);
    /// 5. delete the row (FK cascade drops events/account_data/room_state, and the
    ///    same statement enqueues the search-index purge — ADR 0039).
    ///
    /// Then the identity's lock-map entry is pruned (it is retired for good).
    ///
    /// Idempotent and resumable, keyed by id:
    /// - **`active` / `deactivated` row** → full teardown.
    /// - **`deleting` row** → resume it (a crash or earlier failure left it
    ///   mid-flight); every step is idempotent. This is the branch the boot
    ///   reconcile and a client retry hit.
    /// - **no such row** → [`LifecycleError::NotFound`] (404): already gone. A
    ///   second concurrent delete observes this once the first completes.
    ///
    /// If the supervised task cannot be made to terminate (survives cancel **and**
    /// abort — see [`reap_task`](Self::reap_task)), this fails with
    /// [`LifecycleError::Draining`] **before** the store dir is touched, leaving the
    /// row `deleting` for a retry — a live task's store dir is never removed out
    /// from under it.
    pub async fn delete(&self, account_id: Uuid) -> Result<(), LifecycleError> {
        // Resolve identity to take the per-identity lock; a 404 needs no lock.
        let account = self
            .store
            .get_account(account_id)
            .await?
            .ok_or(LifecycleError::NotFound(account_id))?;

        let lock = self.lock_for(&account.user_id, &account.homeserver_url);
        let _guard = lock.lock().await;

        // Re-read under the lock: a concurrent verb may have moved or removed the
        // row between the unlocked resolve and acquiring the lock.
        let account = self
            .store
            .get_account(account_id)
            .await?
            .ok_or(LifecycleError::NotFound(account_id))?;

        // A QR flow that won the same identity lock owns the account's next
        // session and staging-store finalization. Refuse deletion before its
        // durable row or store path can be removed out from under that flow.
        self.ensure_no_matrix_oauth_acquire(&account.user_id)
            .await?;

        // Flip to `deleting` first (unless already there — a resume). Durably marks
        // that external cleanup is owed, and — like logout's flip — moves the row
        // out of `active` so `get_or_connect`'s cold-connect gate refuses any new
        // client before `sever_session` takes the cached one.
        if account.state != AccountState::Deleting {
            self.store
                .set_account_state(account_id, AccountState::Deleting)
                .await?;
        }

        // Sever the live session. A wedged task returns `Draining` and we stop here
        // — the row stays `deleting`, nothing on disk is touched, and a retry (or
        // the boot reconcile) finishes once the task finally dies. This is why the
        // store-dir removal below sits *after* a successful sever.
        self.sever_session(account_id).await?;

        // External resources before the row (the row is the reconcile's only handle
        // to them): the SDK store dir + its staging backup. Idempotent on a resume.
        crate::client::remove_account_store_dirs(&self.config, account_id).await?;

        // Purge the account's media-cache directory + its LRU index entries (M11,
        // ADR 0024 step 5). Keyed by id like the store dir, so it's another
        // external resource cleared before the row; idempotent (a missing dir is
        // fine), and the boot media orphan-GC is the backstop if this is
        // interrupted or the account was deleted while a prior build had no cache.
        self.media.purge_account(account_id).await;

        // Drop the row. The FK cascade removes events/account_data/room_state, and
        // the same statement appends a durable search-index purge obligation to
        // `search_outbox` (which has no FK, so it outlives the row). That makes the
        // purge crash-safe and independent of whether search is currently enabled
        // (ADR 0039) — a deletion while search is off is healed on the next enabled
        // boot's drain.
        self.store.delete_account_row(account_id).await?;

        // When the indexer is live, flush so the account's documents are actually
        // gone from Tantivy before this verb returns — closing the privacy window
        // synchronously. If it is absent/stopped, the durable obligation above is
        // the backstop, so returning without flushing is still correct.
        if let Some(index) = self.index.as_ref() {
            index.flush().await;
        }

        // The identity is retired for good, so prune its lock-map entry — but only
        // if no other verb is parked on it (see `prune_lock`).
        self.prune_lock(&account.user_id, &account.homeserver_url, &lock);

        tracing::info!(%account_id, user_id = %account.user_id, "account deleted");
        Ok(())
    }

    /// Prune the per-identity lock-map entry for a retired (deleted) identity —
    /// but only when no other verb still holds the lock `Arc`. ADR 0024: pruning
    /// while a verb is *parked* on the same `Arc` would let a later
    /// [`lock_for`](Self::lock_for) mint a *fresh* lock for the identity and run
    /// without mutual exclusion against that waiter. Performed under the std map
    /// mutex so no new waiter can clone the `Arc` between the count check and the
    /// removal.
    fn prune_lock(&self, user_id: &str, _homeserver_url: &str, lock: &Arc<AsyncMutex<()>>) {
        let key = user_id.to_owned();
        let mut map = self.locks.lock().expect("lifecycle lock map poisoned");
        // Live strong refs: the map entry (1) + our own `lock` handle (1) + one per
        // parked waiter. `== 2` ⇒ only map + us, so removing the entry can't strand
        // a waiter on an orphaned lock. `> 2` ⇒ leave it (a tiny bounded leak,
        // reclaimed by the next delete of this identity — correctness over a slot).
        if Arc::strong_count(lock) == 2 {
            map.remove(&key);
        }
    }

    /// Stop the account's supervised task and wait until it has actually
    /// terminated, so on `Ok` the caller may treat the account's SDK store dir
    /// as quiescent (a no-op if no task is registered). Cancellation is
    /// cooperative, so this escalates: cancel → await ([`DRAIN_TIMEOUT`]);
    /// on timeout abort → await ([`ABORT_TIMEOUT`], aborts land at the task's
    /// next await point). A task that survives even the abort — wedged in
    /// non-yielding code — is re-registered and the verb fails with
    /// [`LifecycleError::Draining`]: never "proceed with the task alive", which
    /// would let a re-login restage the store dir out from under it. The
    /// retained entry is what makes [`login`](Self::login) refuse the identity
    /// and a logout retry try the reap again. A join error (panic or abort)
    /// still means the task is gone, which is all the caller needs.
    async fn reap_task(&self, account_id: Uuid) -> Result<(), LifecycleError> {
        // Cleared unconditionally (a no-op if absent), before the cancel/drain/abort
        // sequence below: intentionally so `/v1/status` omits the account rather than
        // show a stale state, even in the `LifecycleError::Draining` case where the
        // task survives and is re-registered. `backfill_health` has no matching call
        // here not from an oversight but because it isn't keyed per account at all —
        // unlike this map, it's a single process-wide disk-space gauge (see
        // `BackfillHealth`), so there's no per-account entry for it to drop.
        self.sync_health.remove(account_id);

        // The map guard is dropped before any await.
        let task = self
            .tasks
            .lock()
            .expect("task registry poisoned")
            .remove(&account_id);
        let Some(AccountTask { cancel, mut handle }) = task else {
            return Ok(());
        };

        cancel.cancel();
        match tokio::time::timeout(DRAIN_TIMEOUT, &mut handle).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(err)) => {
                tracing::warn!(
                    %account_id,
                    error = %err,
                    "supervised task panicked during logout drain"
                );
                return Ok(());
            }
            Err(_) => tracing::warn!(
                %account_id,
                timeout_secs = DRAIN_TIMEOUT.as_secs(),
                "supervised task did not finish draining within the timeout; aborting it"
            ),
        }

        handle.abort();
        match tokio::time::timeout(ABORT_TIMEOUT, &mut handle).await {
            // Finished or cancelled — terminated either way.
            Ok(_) => Ok(()),
            Err(_) => {
                tracing::error!(
                    %account_id,
                    timeout_secs = ABORT_TIMEOUT.as_secs(),
                    "supervised task survived abort (wedged in non-yielding code); \
                     its store dir cannot be treated as free"
                );
                self.tasks
                    .lock()
                    .expect("task registry poisoned")
                    .insert(account_id, AccountTask { cancel, handle });
                Err(LifecycleError::Draining(account_id))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A media-cache handle over a throwaway temp directory, for the lifecycle
    /// tests (the delete path calls `purge_account`, which is a no-op when the
    /// account has no cached media).
    async fn test_media_handle() -> MediaCacheHandle {
        let dir =
            std::env::temp_dir().join(format!("axon-media-lifecycle-test-{}", Uuid::new_v4()));
        axon_media::MediaCache::open(&axon_core::MediaConfig {
            enabled: true,
            cache_dir: dir,
            max_bytes: 1 << 20,
            max_object_bytes: 1 << 20,
            fetch_timeout_secs: 30,
            max_concurrent_downloads: 8,
            ..axon_core::MediaConfig::default()
        })
        .await
        .expect("open media cache")
        .handle()
    }

    /// Build a lifecycle over the test DB. The branches exercised here all return
    /// before any homeserver/SDK contact, so the manager/data_dir are never used.
    async fn lifecycle() -> AccountLifecycle {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
        let store = Store::connect(&url, 5).await.expect("connect + migrate");
        let config = SyncConfig {
            data_dir: std::env::temp_dir().join("axon-lifecycle-test"),
            store_key: Some("test-key".to_owned()),
            timeline_limit: 1,
            live_event_buffer: 16,
            ..SyncConfig::default()
        };
        let manager = ClientManager::new(store.clone(), config.clone());
        let (live_tx, _rx) = broadcast::channel(16);
        AccountLifecycle::new(
            store,
            config,
            manager,
            live_tx,
            CancellationToken::new(),
            TaskTracker::new(),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            crate::verification::new_registry(),
            crate::verification::VerificationRooms::new(),
            None,
            test_media_handle().await,
            crate::backfill::BackfillHealth::new(None),
            crate::sync_health::SyncHealth::new(),
        )
    }

    async fn delete_account(store: &Store, account_id: Uuid) {
        sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
            .bind(account_id)
            .execute(store.pool())
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn qr_reservation_waits_for_a_login_state_decision() {
        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@qr-login-race-{}:localhost", Uuid::new_v4());
        let account = lc.store.upsert_account(&user, hs).await.unwrap();
        lc.store
            .set_account_state(account.account_id, AccountState::Deactivated)
            .await
            .unwrap();

        // Stand in for the password-login critical section: QR creation must
        // wait, then re-read the state written while the shared lock was held.
        let lock = lc.lock_for(&user, hs);
        let guard = lock.lock().await;
        let flow_id = Uuid::new_v4();
        let reserve = tokio::spawn({
            let lc = lc.clone();
            let user = user.clone();
            async move {
                lc.reserve_matrix_oauth_acquire(flow_id, &user, "display", &flow_id.to_string())
                    .await
            }
        });
        tokio::task::yield_now().await;
        lc.store
            .set_account_state(account.account_id, AccountState::Active)
            .await
            .unwrap();
        drop(guard);

        assert!(matches!(
            reserve.await.unwrap().unwrap_err(),
            LifecycleError::AlreadyActive(id) if id == account.account_id
        ));
        assert!(!lc
            .store
            .has_matrix_oauth_acquire_for_user(&user)
            .await
            .unwrap());
        delete_account(&lc.store, account.account_id).await;
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn qr_reservation_wins_before_other_lifecycle_verbs() {
        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@qr-first-race-{}:localhost", Uuid::new_v4());
        let account = lc.store.upsert_account(&user, hs).await.unwrap();
        lc.store
            .set_account_state(account.account_id, AccountState::Deactivated)
            .await
            .unwrap();
        let flow_id = Uuid::new_v4();
        lc.reserve_matrix_oauth_acquire(flow_id, &user, "scan", &flow_id.to_string())
            .await
            .unwrap();

        assert!(matches!(
            lc.login(Some(hs), &user, "not-used").await.unwrap_err(),
            LifecycleError::LoginFinalizing(ref blocked) if blocked == &user
        ));
        assert!(matches!(
            lc.import_token(hs, &user, "not-used", "DEVICE")
                .await
                .unwrap_err(),
            LifecycleError::LoginFinalizing(ref blocked) if blocked == &user
        ));
        assert!(matches!(
            lc.delete(account.account_id).await.unwrap_err(),
            LifecycleError::LoginFinalizing(ref blocked) if blocked == &user
        ));

        assert!(lc
            .store
            .abandon_matrix_oauth_acquire(flow_id)
            .await
            .unwrap());
        delete_account(&lc.store, account.account_id).await;
    }

    /// Login on an already-`active` account is an idempotent no-op: it returns the
    /// existing id, doesn't consult the password, and changes nothing.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn login_on_active_account_is_idempotent_noop() {
        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@noop-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap(); // active by default

        // Deliberately wrong password — an active account never consults it.
        let id = lc
            .login(Some(hs), &user, "not-the-password")
            .await
            .expect("active login is a no-op");
        assert_eq!(id, acct.account_id);

        // Untouched: still active, same row.
        let after = lc
            .store
            .get_account(acct.account_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, AccountState::Active);

        delete_account(&lc.store, acct.account_id).await;
    }

    /// Config and discovery can use different homeserver base URLs for the same
    /// MXID. Login must still return the active configured row rather than minting
    /// a second account.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn login_on_active_user_with_different_homeserver_is_idempotent_noop() {
        let lc = lifecycle().await;
        let configured_hs = "https://matrix.example.org";
        let discovered_hs = "https://client.example.org";
        let user = format!("@url-alias-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, configured_hs).await.unwrap();

        let id = lc
            .login(Some(discovered_hs), &user, "not-the-password")
            .await
            .expect("active Matrix id is a no-op across URL aliases");
        assert_eq!(id, acct.account_id);

        let visible = lc.store.list_client_visible_accounts().await.unwrap();
        assert_eq!(
            visible
                .iter()
                .filter(|account| account.user_id == user)
                .count(),
            1
        );

        delete_account(&lc.store, acct.account_id).await;
    }

    /// Login on a `deleting` row is a conflict (→ 409), not a no-op.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn login_on_deleting_account_conflicts() {
        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@del-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap();
        lc.store
            .set_account_state(acct.account_id, AccountState::Deleting)
            .await
            .unwrap();

        let err = lc.login(Some(hs), &user, "pw").await.unwrap_err();
        assert!(matches!(err, LifecycleError::BeingDeleted(id) if id == acct.account_id));

        delete_account(&lc.store, acct.account_id).await;
    }

    /// A username that isn't a valid full MXID is rejected (→ 400) before any
    /// store/identity work.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn login_with_invalid_mxid_is_rejected() {
        let lc = lifecycle().await;
        let err = lc
            .login(Some("https://hs.example.org"), "not-an-mxid", "pw")
            .await
            .unwrap_err();
        assert!(matches!(err, LifecycleError::InvalidUserId(_)));
    }

    /// `import_token` on an already-`active` account is an idempotent no-op:
    /// it returns the existing id, doesn't consult the token, and changes
    /// nothing.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn import_token_on_active_account_is_idempotent_noop() {
        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@import-noop-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap(); // active by default

        // Deliberately garbage token — an active account never consults it.
        let id = lc
            .import_token(hs, &user, "not-a-real-token", "SOMEDEVICE")
            .await
            .expect("active import is a no-op");
        assert_eq!(id, acct.account_id);

        let after = lc
            .store
            .get_account(acct.account_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, AccountState::Active);

        delete_account(&lc.store, acct.account_id).await;
    }

    /// `import_token` on a `deleting` row is a conflict (→ 409), not a no-op.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn import_token_on_deleting_account_conflicts() {
        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@import-del-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap();
        lc.store
            .set_account_state(acct.account_id, AccountState::Deleting)
            .await
            .unwrap();

        let err = lc
            .import_token(hs, &user, "tok", "SOMEDEVICE")
            .await
            .unwrap_err();
        assert!(matches!(err, LifecycleError::BeingDeleted(id) if id == acct.account_id));

        delete_account(&lc.store, acct.account_id).await;
    }

    /// A username that isn't a valid full MXID is rejected (→ 400) before any
    /// store/identity work.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn import_token_with_invalid_mxid_is_rejected() {
        let lc = lifecycle().await;
        let err = lc
            .import_token("https://hs.example.org", "not-an-mxid", "tok", "SOMEDEVICE")
            .await
            .unwrap_err();
        assert!(matches!(err, LifecycleError::InvalidUserId(_)));
    }

    /// Logout on an already-`deactivated` account is an idempotent no-op: it
    /// succeeds and leaves the row `deactivated`.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn logout_on_deactivated_account_is_idempotent_noop() {
        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@logout-noop-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap();
        lc.store
            .set_account_state(acct.account_id, AccountState::Deactivated)
            .await
            .unwrap();

        lc.logout(acct.account_id)
            .await
            .expect("logout on a deactivated account is a no-op");

        let after = lc
            .store
            .get_account(acct.account_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, AccountState::Deactivated);

        delete_account(&lc.store, acct.account_id).await;
    }

    /// Logout clears `verified` (ADR 0026): a verified account that logs out must
    /// not leave a stale `verified: true` behind, since the session's device is now
    /// dead and a re-login mints a *fresh, unverified* device. Regression for the
    /// "verified account → logout → re-login returns stale true" review finding.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn logout_resets_verified_flag() {
        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@logout-verified-{}:localhost", Uuid::new_v4());
        // Freshly upserted rows are `active`; mark this one verified as a prior
        // recover would have.
        let acct = lc.store.upsert_account(&user, hs).await.unwrap();
        lc.store
            .set_account_verified(acct.account_id, true)
            .await
            .unwrap();

        lc.logout(acct.account_id)
            .await
            .expect("logout on an active account succeeds");

        let after = lc
            .store
            .get_account(acct.account_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, AccountState::Deactivated);
        assert!(
            !after.verified,
            "logout must reset verified to false so a re-login can't read a stale true"
        );

        delete_account(&lc.store, acct.account_id).await;
    }

    /// The factored `lock_and_persist_verified` helper (the watcher's critical
    /// section) takes the per-identity lock *before* its derive+persist, so it
    /// cannot interleave with another holder of the same lock — the property that
    /// closes the `recover` × watcher lost-update race (ADR 0026).
    ///
    /// Deterministic, no ordering sleeps: the test itself acquires the lock first
    /// (program order, not timing), then spawns the helper. While the lock is held
    /// the helper provably cannot complete — joining it with a timeout *must* time
    /// out; if the helper skipped the lock it would finish immediately and the
    /// assertion would fail. After the explicit release it completes and persists
    /// its derived value. (The exact lost-update *value* needs a live homeserver to
    /// drive `derive_verified`, like every other recover test; here the client is
    /// offline, so the derive is a deterministic `false`.) Multi-thread runtime so
    /// the held lock doesn't simply starve the single worker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires Postgres"]
    async fn lock_and_persist_verified_waits_for_the_identity_lock() {
        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@verified-race-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap();
        let id = acct.account_id;
        // Seed a `true` so a missed-lock write (an immediate `false`) is observable.
        lc.store.set_account_verified(id, true).await.unwrap();

        // The helper resolves the *same* lock the verbs use (one shared map).
        let lock = lock_for(&lc.locks, &user, hs);
        let held = lock_for(&lc.locks, &user, hs);
        assert!(Arc::ptr_eq(&lock, &held), "same identity must share a lock");

        // An offline client: `derive_verified` reads no own-device and yields false.
        let client = matrix_sdk::Client::builder()
            .homeserver_url("http://127.0.0.1:9")
            .server_versions([matrix_sdk::ruma::api::MatrixVersion::V1_11])
            .build()
            .await
            .expect("offline client");

        // Acquire the lock here FIRST (program order — no sleep needed to establish
        // "first owns the lock before the second starts").
        let guard = held.lock().await;

        let store = lc.store.clone();
        // A never-cancelled token: this test exercises the lock-wait path, not the
        // cancellation bail-out (covered separately below).
        let never = CancellationToken::new();
        let mut helper = tokio::spawn(async move {
            lock_and_persist_verified(&lock, &client, &store, id, &never).await;
        });

        // While we hold the lock the helper cannot finish: a join attempt must time
        // out. (A no-lock helper would complete here, failing this assertion.)
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut helper)
                .await
                .is_err(),
            "helper must block on the identity lock, not write while it is held"
        );
        // The held value is therefore untouched.
        assert!(lc.store.get_account(id).await.unwrap().unwrap().verified);

        // Release explicitly; now the helper acquires, derives (offline → false),
        // and persists.
        drop(guard);
        helper.await.unwrap();
        assert!(
            !lc.store.get_account(id).await.unwrap().unwrap().verified,
            "after release the helper's under-lock derive is persisted"
        );

        delete_account(&lc.store, acct.account_id).await;
    }

    /// Regression for the shutdown deadlock / stale-write race (ADR 0026): the
    /// watcher's `lock_and_persist_verified` must abandon its lock wait when its
    /// token is cancelled. A lifecycle verb holds the identity lock while it awaits
    /// the watcher's drain, so an un-cancellable wait here would wedge shutdown — and
    /// a detached watcher could then persist the dead device's value over the verb's
    /// reset `false`.
    ///
    /// We stand in for the verb: hold the lock (as logout does), park the helper on
    /// it (as a mid-flight watcher would be), then fire the token. The helper must
    /// complete *promptly while the lock is still held* (proving it didn't block on
    /// the lock) and must **not** write — the seeded `true` survives, so no stale
    /// value lands. Multi-thread so the held lock doesn't starve the worker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires Postgres"]
    async fn lock_and_persist_verified_bails_out_on_cancellation() {
        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@verified-cancel-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap();
        let id = acct.account_id;
        // Seed a `true`: a bail-out must leave it untouched; a write would make it
        // `false` (the offline client derives `false`), so `true` proves no write.
        lc.store.set_account_verified(id, true).await.unwrap();

        let lock = lock_for(&lc.locks, &user, hs);
        let client = matrix_sdk::Client::builder()
            .homeserver_url("http://127.0.0.1:9")
            .server_versions([matrix_sdk::ruma::api::MatrixVersion::V1_11])
            .build()
            .await
            .expect("offline client");

        // Stand in for the lifecycle verb: hold the lock across the drain.
        let guard = lock.lock().await;

        let cancel = CancellationToken::new();
        let store = lc.store.clone();
        let cancel_for_helper = cancel.clone();
        let helper_lock = lock.clone();
        let mut helper = tokio::spawn(async move {
            lock_and_persist_verified(&helper_lock, &client, &store, id, &cancel_for_helper).await;
        });

        // Parked on the lock: it cannot complete while we hold it.
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut helper)
                .await
                .is_err(),
            "helper must be parked on the held identity lock"
        );

        // Cancel WITHOUT releasing the lock — the verb still holds it. A
        // cancellation-aware wait must unpark and return immediately anyway.
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), &mut helper)
            .await
            .expect("cancelled helper must return without waiting for the lock")
            .unwrap();

        // No write happened: the seeded `true` is intact (a deadlock-prone write
        // would have clobbered it with the offline `false`).
        assert!(
            lc.store.get_account(id).await.unwrap().unwrap().verified,
            "a cancelled helper must not persist a (stale) value"
        );

        drop(guard);
        delete_account(&lc.store, acct.account_id).await;
    }

    /// The `RecoveryError` → `LifecycleError` classifier (ADR 0026) reserves the
    /// client-facing `RecoveryFailed` (→ 400) for the specific key/backup-config
    /// secret-storage variants, and routes every internal/upstream variant to
    /// `Upstream` (→ a logged 500). No DB needed — pure mapping. Covers one
    /// actionable and one internal `SecretStorageError` variant, plus a
    /// non-secret-storage `RecoveryError`.
    #[test]
    fn classify_recovery_error_separates_actionable_from_internal() {
        use matrix_sdk::encryption::secret_storage::SecretStorageError as Ss;

        // Actionable: no secret storage configured on the account → 400.
        let actionable =
            classify_recovery_error(RecoveryError::SecretStorage(Ss::MissingKeyInfo {
                key_id: None,
            }));
        match actionable {
            LifecycleError::RecoveryFailed(msg) => {
                assert!(!msg.is_empty());
                // Stable message, not the SDK's own text.
                assert!(!msg.contains("account data"), "must not leak SDK internals");
            }
            other => panic!("missing-key-info should be RecoveryFailed, got {other:?}"),
        }

        // Internal: a nested JSON error inside secret storage → 500 (Upstream).
        let json_err = serde_json::from_str::<i32>("not-an-int").unwrap_err();
        let internal = classify_recovery_error(RecoveryError::SecretStorage(Ss::Json(json_err)));
        assert!(
            matches!(internal, LifecycleError::Upstream(_)),
            "a nested JSON failure is internal, not a client 400"
        );

        // Enable-verb 409: a backup already exists on the homeserver.
        let backup_exists = classify_recovery_error(RecoveryError::BackupExistsOnServer);
        assert!(
            matches!(backup_exists, LifecycleError::BackupConflict(_)),
            "BackupExistsOnServer is 409 Conflict on enable, not 500"
        );
    }

    /// Logout on a `deleting` row is a conflict (→ 409): a delete is in flight.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn logout_on_deleting_account_conflicts() {
        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@logout-del-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap();
        lc.store
            .set_account_state(acct.account_id, AccountState::Deleting)
            .await
            .unwrap();

        let err = lc.logout(acct.account_id).await.unwrap_err();
        assert!(matches!(err, LifecycleError::BeingDeleted(id) if id == acct.account_id));

        delete_account(&lc.store, acct.account_id).await;
    }

    /// Logout on an id with no matching row is a 404, raised before any lock work.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn logout_on_unknown_account_is_not_found() {
        let lc = lifecycle().await;
        let missing = Uuid::new_v4();
        let err = lc.logout(missing).await.unwrap_err();
        assert!(matches!(err, LifecycleError::NotFound(id) if id == missing));
    }

    /// Logout on an `active` row with no live task or cached client (nothing to
    /// cancel or invalidate upstream) still transitions it to `deactivated` — the
    /// state machinery exercised without a homeserver. (The real path, where a live
    /// client is invalidated upstream, is covered manually.)
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn logout_on_active_account_with_no_client_deactivates() {
        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@logout-active-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap(); // active by default

        lc.logout(acct.account_id)
            .await
            .expect("logout on an active account deactivates it");

        let after = lc
            .store
            .get_account(acct.account_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, AccountState::Deactivated);

        delete_account(&lc.store, acct.account_id).await;
    }

    /// Logout must *await* the supervised task's drain, not merely request
    /// cancellation: cancellation is cooperative, and the task keeps using the
    /// account's SQLite store dir while draining — returning early would let an
    /// immediate re-login restage that dir out from under it. Stands in a fake
    /// task whose "drain" (a short post-cancellation sleep) flips a flag; logout
    /// returning with the flag unset is the regression.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn logout_awaits_supervised_task_drain() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use crate::engine::AccountTask;

        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@logout-drain-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap(); // active by default

        let drained = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let handle = tokio::spawn({
            let cancel = cancel.clone();
            let drained = Arc::clone(&drained);
            async move {
                cancel.cancelled().await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                drained.store(true, Ordering::SeqCst);
            }
        });
        lc.tasks
            .lock()
            .unwrap()
            .insert(acct.account_id, AccountTask { cancel, handle });

        lc.logout(acct.account_id).await.expect("logout succeeds");

        assert!(
            drained.load(Ordering::SeqCst),
            "logout returned before the supervised task finished draining"
        );
        assert!(
            !lc.tasks.lock().unwrap().contains_key(&acct.account_id),
            "logout must prune the task-registry entry"
        );
        let after = lc
            .store
            .get_account(acct.account_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, AccountState::Deactivated);

        delete_account(&lc.store, acct.account_id).await;
    }

    /// Regression for the logout/reconnect race: a client sitting in the
    /// account's connection slot at logout time must be taken out, and the row
    /// deactivated *before* the take, so a connect racing the eviction is either
    /// refused by the cold-connect state gate or has its freshly cached client
    /// taken right back out. A cached client left behind would outlive the
    /// deactivation — `get_or_connect` returns a cache hit without re-checking
    /// state — letting a logged-out account keep sending. The injected client is
    /// offline and unauthenticated; its best-effort upstream logout fails fast
    /// and is swallowed by design.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn logout_takes_cached_client_out_of_its_slot() {
        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@logout-evict-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap(); // active by default

        // `server_versions` skips the discovery request, so this builds offline.
        let client = matrix_sdk::Client::builder()
            .homeserver_url("http://127.0.0.1:9") // nothing listens; requests fail fast
            .server_versions([matrix_sdk::ruma::api::MatrixVersion::V1_11])
            .build()
            .await
            .expect("offline client");
        lc.manager.inject_for_test(acct.account_id, client).await;

        lc.logout(acct.account_id).await.expect("logout succeeds");

        assert!(
            lc.manager.take(acct.account_id).await.is_none(),
            "logout must leave no cached client behind"
        );
        let after = lc
            .store
            .get_account(acct.account_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, AccountState::Deactivated);

        delete_account(&lc.store, acct.account_id).await;
    }

    /// A task that ignores cancellation is escalated to an abort: logout still
    /// succeeds — with the task genuinely terminated, not detached — rather than
    /// returning behind a wedged drain.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn logout_aborts_task_that_ignores_cancellation() {
        use crate::engine::AccountTask;

        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@logout-abort-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap(); // active by default

        // Ignores its token entirely; the sleep is an await point, so the abort
        // lands there.
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        lc.tasks
            .lock()
            .unwrap()
            .insert(acct.account_id, AccountTask { cancel, handle });

        lc.logout(acct.account_id)
            .await
            .expect("logout aborts a cancel-ignoring task and succeeds");

        assert!(
            !lc.tasks.lock().unwrap().contains_key(&acct.account_id),
            "the aborted task's registry entry must be pruned"
        );
        let after = lc
            .store
            .get_account(acct.account_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, AccountState::Deactivated);

        delete_account(&lc.store, acct.account_id).await;
    }

    /// Regression for the reap-timeout escape hatch: a task that survives both
    /// cancel and abort (wedged in non-yielding code) must fail the logout with
    /// `Draining` — task re-registered — and a re-login must be refused while it
    /// lives, so nothing can restage the store dir under it. Once the task
    /// finally dies, a logout retry reaps it and clears the refusal.
    /// (Multi-threaded runtime: the wedged task blocks a worker by design.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires Postgres"]
    async fn logout_wedged_task_blocks_relogin_until_reaped() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use crate::engine::AccountTask;

        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@logout-wedged-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap(); // active by default

        // No await points at all, so neither cancellation nor abort can land
        // until `unwedge` is set.
        let unwedge = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let handle = tokio::spawn({
            let unwedge = Arc::clone(&unwedge);
            async move {
                while !unwedge.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        });
        lc.tasks
            .lock()
            .unwrap()
            .insert(acct.account_id, AccountTask { cancel, handle });

        let err = lc.logout(acct.account_id).await.unwrap_err();
        assert!(matches!(err, LifecycleError::Draining(id) if id == acct.account_id));
        assert!(
            lc.tasks.lock().unwrap().contains_key(&acct.account_id),
            "a wedged task must stay registered so re-login keeps being refused"
        );
        // The row still deactivated (the flip precedes the reap)...
        let after = lc
            .store
            .get_account(acct.account_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, AccountState::Deactivated);
        // ...but reactivating it is refused before any store-dir or homeserver
        // work, while the old task may still be using the dir.
        let err = lc.login(Some(hs), &user, "pw").await.unwrap_err();
        assert!(matches!(err, LifecycleError::Draining(id) if id == acct.account_id));

        // Let the task die, then retry: the leftover is reaped and logout
        // completes, clearing the way for a login.
        unwedge.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(50)).await;
        lc.logout(acct.account_id)
            .await
            .expect("retry reaps the now-dead task");
        assert!(!lc.tasks.lock().unwrap().contains_key(&acct.account_id));

        delete_account(&lc.store, acct.account_id).await;
    }

    /// `verified` is cleared even when the logout's sever fails with `Draining`
    /// (ADR 0026). The clear must precede the fallible sever: the row is already
    /// flipped to `deactivated` by then, so a clear placed after `sever_session`
    /// would be skipped on the `Draining` path, leaving a previously-verified
    /// account client-visible as `verified: true` — the exact stale state this is
    /// meant to eliminate. Regression for that ordering. (Multi-threaded runtime:
    /// the wedged task blocks a worker by design.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires Postgres"]
    async fn logout_clears_verified_even_when_draining() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use crate::engine::AccountTask;

        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@logout-drain-verified-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap(); // active by default
        lc.store
            .set_account_verified(acct.account_id, true)
            .await
            .unwrap();

        // No await points, so neither cancel nor abort lands until `unwedge`: the
        // sever wedges and logout returns `Draining`.
        let unwedge = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let handle = tokio::spawn({
            let unwedge = Arc::clone(&unwedge);
            async move {
                while !unwedge.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        });
        lc.tasks
            .lock()
            .unwrap()
            .insert(acct.account_id, AccountTask { cancel, handle });

        let err = lc.logout(acct.account_id).await.unwrap_err();
        assert!(matches!(err, LifecycleError::Draining(id) if id == acct.account_id));

        let after = lc
            .store
            .get_account(acct.account_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, AccountState::Deactivated);
        assert!(
            !after.verified,
            "verified must be cleared before the fallible sever, even on Draining"
        );

        // Let the wedged task die so the test runtime can shut down cleanly.
        unwedge.store(true, Ordering::SeqCst);
        delete_account(&lc.store, acct.account_id).await;
    }

    // ---- delete (ADR 0024) ----

    /// Delete of an `active` account removes the row, its on-disk SDK store dir
    /// and staging backup, and retires the identity (a later login would mint a
    /// fresh id, since `find_account_by_user_id` now returns `None`).
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn delete_on_active_removes_row_and_store_dir() {
        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@delete-active-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap(); // active

        // Stand in an on-disk store dir + staging backup so we can assert both go.
        let dir = lc.config.data_dir.join(acct.account_id.to_string());
        let backup = lc.config.data_dir.join(format!("{}.prev", acct.account_id));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&backup).unwrap();

        lc.delete(acct.account_id).await.expect("delete succeeds");

        assert!(
            lc.store
                .get_account(acct.account_id)
                .await
                .unwrap()
                .is_none(),
            "row removed"
        );
        assert!(!dir.exists(), "store dir removed");
        assert!(!backup.exists(), "staging backup removed");
        assert!(
            lc.store
                .find_account_by_user_id(&user)
                .await
                .unwrap()
                .is_none(),
            "identity retired — a fresh login would mint a new id"
        );
    }

    /// Delete of a `deactivated` account also fully removes it (retained archive
    /// and all). Logout keeps data; delete erases it.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn delete_on_deactivated_removes_row() {
        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@delete-deact-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap();
        lc.store
            .set_account_state(acct.account_id, AccountState::Deactivated)
            .await
            .unwrap();

        lc.delete(acct.account_id).await.expect("delete succeeds");
        assert!(lc
            .store
            .get_account(acct.account_id)
            .await
            .unwrap()
            .is_none());
    }

    /// Delete of a row already in `deleting` (a crash/failure left it mid-flight)
    /// **resumes** the teardown to completion rather than erroring — the branch the
    /// boot reconcile and a client retry both take.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn delete_on_deleting_row_resumes_to_completion() {
        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@delete-resume-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap();
        lc.store
            .set_account_state(acct.account_id, AccountState::Deleting)
            .await
            .unwrap();
        let dir = lc.config.data_dir.join(acct.account_id.to_string());
        std::fs::create_dir_all(&dir).unwrap();

        lc.delete(acct.account_id).await.expect("resume completes");
        assert!(lc
            .store
            .get_account(acct.account_id)
            .await
            .unwrap()
            .is_none());
        assert!(!dir.exists());
    }

    /// Delete twice: the first removes the row, the second finds nothing — the
    /// shape a second concurrent delete sees once the first wins the identity lock.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn delete_is_idempotent_then_not_found() {
        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@delete-twice-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap();

        lc.delete(acct.account_id).await.expect("first delete");
        let err = lc.delete(acct.account_id).await.unwrap_err();
        assert!(matches!(err, LifecycleError::NotFound(id) if id == acct.account_id));
    }

    /// Delete on an unknown id is a 404, raised before any lock work.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn delete_on_unknown_account_is_not_found() {
        let lc = lifecycle().await;
        let missing = Uuid::new_v4();
        let err = lc.delete(missing).await.unwrap_err();
        assert!(matches!(err, LifecycleError::NotFound(id) if id == missing));
    }

    /// Delete reaps the account's supervised task (awaiting its drain) before
    /// removing anything, then completes — mirrors the logout drain test.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn delete_reaps_supervised_task_then_completes() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use crate::engine::AccountTask;

        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@delete-drain-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap();

        let drained = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let handle = tokio::spawn({
            let cancel = cancel.clone();
            let drained = Arc::clone(&drained);
            async move {
                cancel.cancelled().await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                drained.store(true, Ordering::SeqCst);
            }
        });
        lc.tasks
            .lock()
            .unwrap()
            .insert(acct.account_id, AccountTask { cancel, handle });

        lc.delete(acct.account_id).await.expect("delete succeeds");

        assert!(
            drained.load(Ordering::SeqCst),
            "delete returned before the supervised task finished draining"
        );
        assert!(!lc.tasks.lock().unwrap().contains_key(&acct.account_id));
        assert!(lc
            .store
            .get_account(acct.account_id)
            .await
            .unwrap()
            .is_none());
    }

    /// Regression for the load-bearing ordering: a task that survives cancel **and**
    /// abort fails the delete with `Draining` **before** the store dir is touched —
    /// the row stays `deleting` and the on-disk store is left intact, so nothing is
    /// removed out from under a still-live task. A retry once the task dies finishes
    /// the job.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires Postgres"]
    async fn delete_wedged_task_is_draining_and_preserves_store_dir() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use crate::engine::AccountTask;

        let lc = lifecycle().await;
        let hs = "https://hs.example.org";
        let user = format!("@delete-wedged-{}:localhost", Uuid::new_v4());
        let acct = lc.store.upsert_account(&user, hs).await.unwrap();
        let dir = lc.config.data_dir.join(acct.account_id.to_string());
        std::fs::create_dir_all(&dir).unwrap();

        // No await points: survives both cancel and abort until `unwedge` is set.
        let unwedge = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let handle = tokio::spawn({
            let unwedge = Arc::clone(&unwedge);
            async move {
                while !unwedge.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        });
        lc.tasks
            .lock()
            .unwrap()
            .insert(acct.account_id, AccountTask { cancel, handle });

        let err = lc.delete(acct.account_id).await.unwrap_err();
        assert!(matches!(err, LifecycleError::Draining(id) if id == acct.account_id));
        // Row left `deleting`, task still registered, and — critically — the store
        // dir is untouched (the teardown aborted before the removal step).
        let after = lc
            .store
            .get_account(acct.account_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, AccountState::Deleting);
        assert!(lc.tasks.lock().unwrap().contains_key(&acct.account_id));
        assert!(dir.exists(), "a live task's store dir must not be removed");

        // Let it die and retry: the leftover is reaped and the delete completes.
        unwedge.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(50)).await;
        lc.delete(acct.account_id).await.expect("retry completes");
        assert!(lc
            .store
            .get_account(acct.account_id)
            .await
            .unwrap()
            .is_none());
        assert!(!dir.exists());
    }

    /// The lock-map prune guard (ADR 0024): pruning removes the identity's entry
    /// only when no other verb still holds the lock `Arc`. A parked waiter (a live
    /// extra clone) must keep the entry alive, or it would let a fresh `lock_for`
    /// mint a second lock and break mutual exclusion.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn prune_lock_keeps_entry_while_a_waiter_holds_it() {
        let lc = lifecycle().await;
        let user = format!("@prune-{}:localhost", Uuid::new_v4());
        let hs = "https://hs.example.org";
        let key = user.clone();

        // `lock_for` inserts the entry and returns a clone: map(1) + ours(1).
        let lock = lc.lock_for(&user, hs);

        // A second live clone stands in for a parked waiter — strong_count is now 3,
        // so the guard must refuse to prune.
        let waiter = lock.clone();
        lc.prune_lock(&user, hs, &lock);
        assert!(
            lc.locks.lock().unwrap().contains_key(&key),
            "must not prune while a waiter holds the lock"
        );

        // Drop the waiter (back to map + us = 2): now pruning is safe and removes it.
        drop(waiter);
        lc.prune_lock(&user, hs, &lock);
        assert!(
            !lc.locks.lock().unwrap().contains_key(&key),
            "uncontended prune removes the entry"
        );
    }
}
