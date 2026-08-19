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

use crate::client::{connect_account, import_token_new_device, login_new_device};
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
/// The set only ever grows within a process. That is the same accepted
/// tradeoff [`SdkGateway`](crate::gateway)'s power-level locks make: one small
/// entry per room the homeserver has disowned, and nothing here owns room
/// bookkeeping. It does not need to persist — the gateway also drops the room
/// from the SDK's state store, so a restart rebuilds an SDK view that no longer
/// carries the room at all.
type DeadRooms = Arc<Mutex<HashSet<(Uuid, OwnedRoomId)>>>;

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
            dead_rooms: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Record that the homeserver denied knowing `room_id` for this account
    /// (ADR 0094). Called by the gateway when a membership verb comes back
    /// `404 M_NOT_FOUND`/`M_UNKNOWN`.
    pub(crate) fn mark_room_dead(&self, account_id: Uuid, room_id: OwnedRoomId) {
        self.dead_rooms
            .lock()
            .expect("dead room set poisoned")
            .insert((account_id, room_id));
    }

    /// Whether the homeserver has denied knowing this room for this account.
    /// The invite watcher consults this before persisting a `room_invites`
    /// row, because the SDK's in-memory room list still carries the room until
    /// the process restarts (ADR 0094).
    pub(crate) fn is_room_dead(&self, account_id: Uuid, room_id: &RoomId) -> bool {
        self.dead_rooms
            .lock()
            .expect("dead room set poisoned")
            .contains(&(account_id, room_id.to_owned()))
    }

    /// Every room this account's homeserver has disowned, for the invite
    /// watcher's sweep.
    pub(crate) fn dead_rooms_for(&self, account_id: Uuid) -> Vec<OwnedRoomId> {
        self.dead_rooms
            .lock()
            .expect("dead room set poisoned")
            .iter()
            .filter(|(id, _)| *id == account_id)
            .map(|(_, room_id)| room_id.clone())
            .collect()
    }

    /// Fetch (or create) the connection slot for `account_id`.
    fn slot(&self, account_id: Uuid) -> Slot {
        let mut map = self.slots.lock().expect("client slot map poisoned");
        map.entry(account_id).or_default().clone()
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
            return Ok(client.clone());
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
    pub async fn evict(&self, account_id: Uuid) {
        let slot = {
            let map = self.slots.lock().expect("client slot map poisoned");
            map.get(&account_id).cloned()
        };
        if let Some(slot) = slot {
            *slot.lock().await = None;
        }
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
}
