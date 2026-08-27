//! Per-account [`Client`] lifecycle: the single authority on whether a client
//! exists for an account and how it is built.
//!
//! [`ClientManager`] owns connection only — building the SQLite-backed
//! [`Client`], authenticating it (login on first boot, session restore
//! thereafter, via [`connect_account`](crate::client::connect_account)), and
//! caching one Arc-backed client per `account_id`. It runs no retry loop of its
//! own: the sync supervisor (see [`engine`](crate::engine)) is the always-on
//! caller that keeps each account online via its backoff loop, and the message
//! gateway ([`SdkGateway`](crate::gateway)) is an occasional lazy caller. Both go
//! through [`get_or_connect`](ClientManager::get_or_connect), whose per-account
//! single-flight guard ensures concurrent callers coalesce onto one connect
//! rather than building two clients.
//!
//! Message semantics (send/edit/redact/react) deliberately live in a separate
//! type so this one stays purely about connections (single responsibility).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axon_core::SyncConfig;
use axon_store::{Account, AccountState, Store};
use matrix_sdk::ruma::{OwnedRoomId, RoomId};
use matrix_sdk::Client;
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use crate::client::{
    adopt_matrix_oauth_acquire_staging, connect_account, finish_matrix_oauth_acquire_adoption,
    import_token_new_device, login_new_device,
};
use crate::error::{GatewayError, SyncError};

/// A per-account connection slot. The slot's [`AsyncMutex`] is what makes a
/// connect single-flight: the first caller holds it across the (awaiting)
/// connect while later callers for the same account wait, then observe the
/// freshly cached client instead of starting their own connect. `None` means
/// "not connected yet"; `Some` caches the live client (clones are cheap — the
/// SDK client is Arc-backed).
type Slot = Arc<AsyncMutex<Option<Client>>>;

/// Rooms the homeserver has positively denied knowing (ADR 0094), keyed by the
/// account that asked. Shared between the gateway — which learns it from a
/// membership call's `404` — and the invite watcher, which must not re-persist
/// a `room_invites` row for one.
///
/// Keyed `account_id → rooms` rather than as a flat set of pairs: the invite
/// watcher asks "every dead room for *this* account" once per sweep tick, and
/// a flat set makes that a scan-and-clone over every account's entries
/// combined.
///
/// This exists only to shadow one live [`Client`]'s in-memory room list, which
/// keeps reporting a disowned room as `Invited` because matrix-sdk offers no
/// way to evict one. So its lifetime is the client's, not the process's:
/// [`evict`](ClientManager::evict) drops the account's entries along with the
/// client, and the durable half of the evidence — the gateway's
/// `StateStore::remove_room` — is what a rebuilt client reads instead. That
/// bounds the map by *live* accounts rather than by accumulated disowned rooms
/// over uptime.
///
/// If that state-store removal had failed, a rebuilt client resurrects the
/// room and the row returns. That is the direction ADR 0091 chose: a stale row
/// is visible and the user can clear it; a wrongly-deleted one is silent.
type DeadRooms = Arc<Mutex<HashMap<Uuid, HashSet<OwnedRoomId>>>>;

/// Owns and caches one matrix-rust-sdk [`Client`] per account. Cheap to
/// [`Clone`] — every field is a handle — so it is shared by both the sync
/// supervisor and the message gateway.
#[derive(Clone)]
pub struct ClientManager {
    store: Store,
    config: SyncConfig,
    /// `account_id → slot`. The outer (std) mutex is held only briefly to fetch
    /// or insert a slot; the awaiting connect happens under the slot's own async
    /// mutex, so connects for different accounts never block each other and a
    /// connect never blocks the map.
    slots: Arc<Mutex<HashMap<Uuid, Slot>>>,
    /// Accounts whose adopted Matrix OAuth store opened successfully but whose
    /// displaced-store cleanup failed. Cached-client calls retry only these
    /// entries; a cold connect always checks once so restart recovery does not
    /// depend on this process-local set.
    pending_adoption_cleanup: Arc<Mutex<HashSet<Uuid>>>,
    /// Rooms the homeserver says it does not know. See [`DeadRooms`].
    dead_rooms: DeadRooms,
}

impl ClientManager {
    /// Build a manager over the store handle and sync config. No clients are
    /// connected until [`get_or_connect`](Self::get_or_connect) is first called
    /// for an account.
    pub fn new(store: Store, config: SyncConfig) -> Self {
        Self {
            store,
            config,
            slots: Arc::new(Mutex::new(HashMap::new())),
            pending_adoption_cleanup: Arc::new(Mutex::new(HashSet::new())),
            dead_rooms: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn finish_adoption_cleanup(&self, account_id: Uuid) {
        match finish_matrix_oauth_acquire_adoption(&self.config, account_id).await {
            Ok(()) => {
                self.pending_adoption_cleanup
                    .lock()
                    .expect("pending Matrix OAuth adoption cleanup set poisoned")
                    .remove(&account_id);
            }
            Err(error) => {
                self.pending_adoption_cleanup
                    .lock()
                    .expect("pending Matrix OAuth adoption cleanup set poisoned")
                    .insert(account_id);
                tracing::warn!(
                    %account_id,
                    error = %error,
                    "failed to remove previous Matrix OAuth SDK store; will retry"
                );
            }
        }
    }

    async fn retry_pending_adoption_cleanup(&self, account_id: Uuid) {
        let pending = self
            .pending_adoption_cleanup
            .lock()
            .expect("pending Matrix OAuth adoption cleanup set poisoned")
            .contains(&account_id);
        if pending {
            self.finish_adoption_cleanup(account_id).await;
        }
    }

    /// Record that the homeserver denied knowing `room_id` for this account
    /// (ADR 0094). Called by the gateway when a membership verb comes back
    /// `404 M_NOT_FOUND`/`M_UNKNOWN`.
    pub(crate) fn mark_room_dead(&self, account_id: Uuid, room_id: OwnedRoomId) {
        self.dead_rooms
            .lock()
            .expect("dead room set poisoned")
            .entry(account_id)
            .or_default()
            .insert(room_id);
    }

    /// Whether the homeserver has denied knowing this room for this account.
    /// The invite watcher consults this before persisting a `room_invites`
    /// row, because the SDK's in-memory room list still carries the room until
    /// the process restarts (ADR 0094).
    pub(crate) fn is_room_dead(&self, account_id: Uuid, room_id: &RoomId) -> bool {
        self.dead_rooms
            .lock()
            .expect("dead room set poisoned")
            .get(&account_id)
            .is_some_and(|rooms| rooms.contains(room_id))
    }

    /// Every room this account's homeserver has disowned, for the invite
    /// watcher's sweep.
    ///
    /// Reads the set; it is not drained. A dead room must keep suppressing the
    /// `room_invites` row for as long as this client's in-memory list still
    /// reports it as invited, which is until the client is evicted or the
    /// process restarts.
    pub(crate) fn dead_rooms_for(&self, account_id: Uuid) -> Vec<OwnedRoomId> {
        self.dead_rooms
            .lock()
            .expect("dead room set poisoned")
            .get(&account_id)
            .map(|rooms| rooms.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Fetch (or create) the connection slot for `account_id`.
    fn slot(&self, account_id: Uuid) -> Slot {
        let mut map = self.slots.lock().expect("client slot map poisoned");
        map.entry(account_id).or_default().clone()
    }

    /// Peek the cached client for `account_id` without connecting and without
    /// waiting on an in-flight connect. Used by the GET/list backup snapshot
    /// (ADR 0098): a cold connect or a hung slot lock must not pin account
    /// reads. `None` if nothing is cached or the slot is currently locked.
    pub fn cached(&self, account_id: Uuid) -> Option<Client> {
        let slot = self.slot(account_id);
        slot.try_lock().ok().and_then(|guard| guard.clone())
    }

    /// Return the cached client for `account_id`, building and authenticating one
    /// if the account isn't connected yet. Single-flight per account: concurrent
    /// callers coalesce onto a single connect.
    ///
    /// An unknown account id is [`GatewayError::UnknownAccount`]; a non-`active`
    /// account is [`GatewayError::AccountNotActive`] (not retryable without a
    /// login); a connect that fails (homeserver unreachable, auth/restore error,
    /// store error) is [`GatewayError::NotConnected`] — retryable.
    pub async fn get_or_connect(&self, account_id: Uuid) -> Result<Client, GatewayError> {
        let slot = self.slot(account_id);
        let mut guard = slot.lock().await;
        if let Some(client) = guard.as_ref() {
            let client = client.clone();
            drop(guard);
            self.retry_pending_adoption_cleanup(account_id).await;
            return Ok(client);
        }

        let account = self
            .store
            .get_account(account_id)
            .await
            .map_err(|e| GatewayError::NotConnected(e.to_string()))?
            .ok_or(GatewayError::UnknownAccount(account_id))?;

        // Only `active` accounts get a *new* client. The supervisor lists only
        // active rows, but the gateway connects lazily for any id an API send
        // names, so the cold-connect gate lives here too. Note this runs only on a
        // cache miss: an account that already has a cached client (above) is not
        // re-checked, so this gate alone doesn't sever a *connected* account on a
        // state change. The lifecycle verbs do that actively — logout/delete flip
        // the row out of `active` *first* (so this gate refuses any new connect),
        // then stop the account's supervised task and take its cached client out
        // of the slot (see `take`) (ADR 0022).
        if account.state != AccountState::Active {
            return Err(GatewayError::AccountNotActive(account_id));
        }

        let client = connect_account(&self.store, &account, &self.config).await?;
        self.finish_adoption_cleanup(account_id).await;
        *guard = Some(client.clone());
        Ok(client)
    }

    /// Log `account` in as a fresh device and cache the resulting live client in
    /// its connection slot, so the supervised sync task's first
    /// [`get_or_connect`](Self::get_or_connect) reuses this client rather than
    /// building a second one (ADR 0021). The lifecycle layer calls this under its
    /// per-identity lock, having already resolved/minted the row; the caller flips
    /// the row to `active` and spawns the task only on success.
    ///
    /// Holding the slot lock across the login makes it single-flight with any
    /// concurrent `get_or_connect`. Any previously cached client is discarded
    /// first (a reactivated row may carry a stale slot from a prior session).
    pub async fn login(&self, account: &Account, password: &str) -> Result<Client, SyncError> {
        let slot = self.slot(account.account_id);
        let mut guard = slot.lock().await;
        *guard = None;
        let client = login_new_device(&self.store, account, &self.config, password).await?;
        *guard = Some(client.clone());
        Ok(client)
    }

    /// Adopt an existing `access_token` + `device_id` as a fresh device session
    /// for `account` and cache the resulting live client, exactly like
    /// [`login`](Self::login) but for a pre-existing token instead of a
    /// password. Same single-flight/cache-discard semantics.
    pub async fn import_token(
        &self,
        account: &Account,
        access_token: &str,
        device_id: &str,
    ) -> Result<Client, SyncError> {
        let slot = self.slot(account.account_id);
        let mut guard = slot.lock().await;
        *guard = None;
        let client =
            import_token_new_device(&self.store, account, &self.config, access_token, device_id)
                .await?;
        *guard = Some(client.clone());
        Ok(client)
    }

    /// Promote an authenticated Matrix OAuth QR-login client from its staging
    /// store into `account`'s permanent connection slot.
    ///
    /// The acquisition client is built with SQLite paths under
    /// `.matrix-oauth-acquire`; renaming that directory does not rewrite the
    /// paths held by its connection pools. Drop it before the rename, then build
    /// and restore a fresh client from the durable session at the permanent
    /// account path. Holding the slot lock across the whole handoff keeps this
    /// single-flight with every lazy connection attempt. The row remains
    /// `deactivated` until the lifecycle finalizer activates it afterward.
    ///
    /// A failed first restore after a successful adoption is not a promotion
    /// failure. The lifecycle finalizer can still activate and supervise the
    /// durable account, and the ordinary supervisor will retry the cold connect
    /// with backoff. The displaced store remains available until one of those
    /// attempts opens the adopted store and completes its retryable cleanup.
    pub(crate) async fn promote_matrix_oauth_acquire(
        &self,
        account: &Account,
        staging_dir_name: &str,
        acquisition_client: Client,
    ) -> Result<(), SyncError> {
        let slot = self.slot(account.account_id);
        let mut guard = slot.lock().await;
        *guard = None;
        drop(acquisition_client);

        adopt_matrix_oauth_acquire_staging(&self.config, staging_dir_name, account.account_id)
            .await?;
        match connect_account(&self.store, account, &self.config).await {
            Ok(permanent_client) => {
                self.finish_adoption_cleanup(account.account_id).await;
                *guard = Some(permanent_client);
            }
            Err(error) => {
                self.pending_adoption_cleanup
                    .lock()
                    .expect("pending Matrix OAuth adoption cleanup set poisoned")
                    .insert(account.account_id);
                tracing::warn!(
                    account_id = %account.account_id,
                    user_id = %account.user_id,
                    error = %error,
                    "adopted Matrix OAuth SDK store but initial restore failed; supervisor will retry"
                );
            }
        }
        Ok(())
    }

    /// Take the cached client for `account_id` out of its slot, returning it (or
    /// `None` if nothing is cached). This both *yields* the client — so the caller
    /// can do one last thing with it, e.g. invalidate the device token upstream on
    /// logout — and *evicts* it (the slot is left empty), atomically under the slot
    /// lock so a concurrent [`get_or_connect`](Self::get_or_connect) can't observe a
    /// half-removed client. Unlike [`evict`](Self::evict), which drops the client,
    /// this one returns it.
    pub async fn take(&self, account_id: Uuid) -> Option<Client> {
        let slot = self.slot(account_id);
        let mut guard = slot.lock().await;
        guard.take()
    }

    /// Put a client straight into an account's slot, standing in for a connect.
    /// Lets a test exercise the cached-client paths (eviction on logout, the
    /// cache-hit fast path) without a reachable homeserver.
    #[cfg(test)]
    pub(crate) async fn inject_for_test(&self, account_id: Uuid, client: Client) {
        let slot = self.slot(account_id);
        *slot.lock().await = Some(client);
    }

    /// Drop the cached client for `account_id` so the next
    /// [`get_or_connect`](Self::get_or_connect) rebuilds it. Called by the sync
    /// supervisor when a run fails, so a supervised restart reconnects cleanly.
    /// A no-op if nothing is cached.
    ///
    /// Awaits the slot lock rather than skipping when it's held (unlike a
    /// `try_lock`, which would let a concurrent `get_or_connect`'s cache-hit read
    /// win the race and leave the stale, about-to-be-replaced client cached —
    /// the next supervised run would then reuse it and pile a second set of
    /// event handlers onto it; see issue #289).
    ///
    /// Also drops the account's [`DeadRooms`] entries. They only ever existed
    /// to shadow the evicted client's stale in-memory room list; the rebuilt
    /// client reads the SDK state store the gateway already removed the room
    /// from, so re-deriving from scratch is both correct and what keeps the
    /// map bounded by live accounts instead of by process uptime.
    pub async fn evict(&self, account_id: Uuid) {
        let slot = {
            let map = self.slots.lock().expect("client slot map poisoned");
            map.get(&account_id).cloned()
        };
        if let Some(slot) = slot {
            *slot.lock().await = None;
        }
        self.dead_rooms
            .lock()
            .expect("dead room set poisoned")
            .remove(&account_id);
    }

    /// Expose an account's raw connection slot, so a test can hold its lock
    /// itself to simulate a concurrent `get_or_connect`/`login` in flight.
    #[cfg(test)]
    pub(crate) fn slot_for_test(&self, account_id: Uuid) -> Slot {
        self.slot(account_id)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Build a manager over the test DB. `evict`/`inject_for_test` never touch
    /// the store, so no account row is needed — only a live `Store` to satisfy
    /// `ClientManager::new`.
    async fn manager() -> ClientManager {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
        let store = Store::connect(&url, 5).await.expect("connect + migrate");
        let config = SyncConfig {
            data_dir: std::env::temp_dir().join("axon-manager-test"),
            store_key: Some("test-key".to_owned()),
            timeline_limit: 1,
            live_event_buffer: 16,
            ..SyncConfig::default()
        };
        ClientManager::new(store, config)
    }

    /// `server_versions` skips the discovery request, so this builds offline.
    async fn offline_client() -> Client {
        Client::builder()
            .homeserver_url("http://127.0.0.1:9") // nothing listens; requests fail fast
            .server_versions([matrix_sdk::ruma::api::MatrixVersion::V1_11])
            .build()
            .await
            .expect("offline client")
    }

    /// Regression for issue #289: the old `evict` used `try_lock` and silently
    /// skipped when the slot was held by a concurrent `get_or_connect`, leaving
    /// the stale client cached for the next supervised restart to reuse (and
    /// pile a second set of event handlers onto). `evict` must instead await
    /// the lock and still clear the slot once the holder releases it.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn evict_awaits_a_held_slot_lock_instead_of_skipping() {
        let manager = manager().await;
        let account_id = Uuid::new_v4();
        manager
            .inject_for_test(account_id, offline_client().await)
            .await;

        // Simulate a concurrent `get_or_connect` holding the slot lock.
        let slot = manager.slot_for_test(account_id);
        let guard = slot.lock().await;

        let evicting_manager = manager.clone();
        let evict = tokio::spawn(async move {
            evicting_manager.evict(account_id).await;
        });

        // Give `evict` a chance to run; a `try_lock`-based implementation would
        // already have returned by now instead of blocking on the held lock.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !evict.is_finished(),
            "evict must block while the slot lock is held, not skip"
        );

        drop(guard);
        evict.await.expect("evict task panicked");

        assert!(
            manager.take(account_id).await.is_none(),
            "evict must clear the slot once the lock is released"
        );
    }

    /// The dead-room set (ADR 0094) is per-account and per-client: one
    /// account's disowned room must not be visible to another, and evicting a
    /// client must take its shadow of that client's in-memory room list with
    /// it. Without the purge in `evict` the map would grow with account churn
    /// for the life of the process.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn dead_rooms_are_scoped_to_one_account_and_die_with_its_client() {
        let manager = manager().await;
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let disowned = RoomId::parse("!disowned:example.org").expect("valid room id");
        let live = RoomId::parse("!live:example.org").expect("valid room id");

        manager.inject_for_test(first, offline_client().await).await;
        manager.mark_room_dead(first, disowned.to_owned());

        assert!(manager.is_room_dead(first, &disowned));
        assert!(
            !manager.is_room_dead(first, &live),
            "only the room the homeserver disowned is dead"
        );
        assert!(
            !manager.is_room_dead(second, &disowned),
            "one account's 404 says nothing about another account's room"
        );
        assert_eq!(manager.dead_rooms_for(first), vec![disowned.to_owned()]);
        assert!(manager.dead_rooms_for(second).is_empty());

        // Reading the set must not drain it: the SDK keeps reporting the room
        // as invited until the client goes away, so every sweep tick needs the
        // same answer.
        assert_eq!(manager.dead_rooms_for(first), vec![disowned.to_owned()]);

        manager.evict(first).await;
        assert!(
            !manager.is_room_dead(first, &disowned),
            "evict must drop the shadow along with the client it shadowed"
        );
        assert!(manager.dead_rooms_for(first).is_empty());
    }
}
