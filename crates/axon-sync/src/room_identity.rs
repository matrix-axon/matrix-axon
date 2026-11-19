//! Room-member identity warnings from the Matrix SDK (ADR 0136, decision 3).
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};

use axon_core::{IdentityChangeKind, RoomDetailSource, RoomIdentityChange};
use axon_store::{AccountState, Store};
use futures_util::{FutureExt, StreamExt};
use matrix_sdk::{
    encryption::identities::IdentityUpdates,
    ruma::{events::room::member::MembershipState, OwnedRoomId, OwnedUserId, RoomId, UserId},
    Client, Room, RoomState,
};
use matrix_sdk_base::crypto::{
    IdentityState, IdentityStatusChange, RoomIdentityProvider, UserIdentity,
};
use sha2::{Digest, Sha256};
use tokio::{sync::Semaphore, task::JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber;
use uuid::Uuid;

use crate::{
    homeserver_gate::{outcome_of, HomeserverGate},
    lifecycle::{lock_for, IdentityLocks},
    manager::ClientManager,
    room_details::RoomDetailsChanges,
};

const MEMBER_SYNC_TIMEOUT: Duration = Duration::from_secs(10);
const MEMBER_SYNC_ADMISSION_WAIT: Duration = Duration::from_millis(250);
const LOCAL_READ_TIMEOUT: Duration = Duration::from_secs(2);
const RESCAN_INTERVAL: Duration = Duration::from_secs(30);
const RESUBSCRIBE_INTERVAL: Duration = Duration::from_secs(15 * 60);

type Warnings = BTreeMap<OwnedUserId, IdentityChangeKind>;
type Rooms = HashMap<OwnedRoomId, Warnings>;

/// Each supervised account run owns its entries. HTTP readers take snapshots
/// without holding the lock across SDK work.
#[derive(Clone, Default)]
pub(crate) struct RoomIdentityChanges(Arc<Mutex<HashMap<Uuid, Rooms>>>);

impl RoomIdentityChanges {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, Rooms>> {
        self.0.lock().expect("room identity warning map poisoned")
    }

    fn warnings(&self, account_id: Uuid, room_id: &RoomId) -> Option<Warnings> {
        self.lock().get(&account_id)?.get(room_id).cloned()
    }

    fn warning_rooms(&self, account_id: Uuid, user_id: &UserId) -> Vec<OwnedRoomId> {
        self.lock()
            .get(&account_id)
            .into_iter()
            .flatten()
            .filter(|(_, warnings)| warnings.contains_key(user_id))
            .map(|(room_id, _)| room_id.clone())
            .collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RoomIdentityError {
    #[error("invalid Matrix user id or identity fingerprint")]
    InvalidInput,
    #[error("the account or user identity was not found")]
    NotFound,
    #[error("the account is not active or its crypto store is not ready")]
    NotReady,
    #[error("the identity warning changed; refresh room info and review it again")]
    Conflict,
    #[error("the room identity change could not be applied")]
    Internal,
}

/// Reads the supervised projection and applies account-wide acknowledgements.
#[derive(Clone)]
pub struct RoomIdentityEngine {
    pub(crate) store: Store,
    pub(crate) manager: ClientManager,
    pub(crate) locks: IdentityLocks,
}

impl RoomIdentityEngine {
    /// `None` means there is no current snapshot (e.g. a cold or left room).
    pub async fn warnings(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Option<Vec<RoomIdentityChange>> {
        let room_id = <&RoomId>::try_from(room_id).ok()?;
        let cache = self.manager.room_identity_changes();
        let warnings = cache.warnings(account_id, room_id)?;
        let client = self.manager.cached(account_id)?;
        let room = client.get_room(room_id)?;
        if !eligible(&room) {
            return None;
        }

        let result = tokio::time::timeout(LOCAL_READ_TIMEOUT, async {
            let mut snapshot = Vec::new();
            for user_id in warnings.keys() {
                if !active_member(&room, user_id).await? {
                    continue;
                }
                let Some(identity) = room.user_identity(user_id).await else {
                    continue;
                };
                if let Some((kind, fingerprint)) = current_warning(&room, &identity) {
                    snapshot.push(RoomIdentityChange {
                        user_id: user_id.to_string(),
                        kind,
                        fingerprint,
                    });
                }
            }
            Ok::<_, matrix_sdk::Error>(snapshot)
        })
        .await;
        match result {
            Ok(Ok(snapshot)) => Some(snapshot),
            _ => {
                tracing::warn!(%account_id, %room_id, "room identity warning read failed");
                None
            }
        }
    }

    /// Resolve the warning for the exact public identity the client reviewed.
    /// Share the lifecycle lock with login, logout, and other verification verbs.
    pub async fn resolve(
        &self,
        account_id: Uuid,
        user_id: &str,
        expected_fingerprint: &str,
        kind: IdentityChangeKind,
    ) -> Result<(), RoomIdentityError> {
        use RoomIdentityError::{Conflict, Internal, InvalidInput, NotFound, NotReady};

        let user_id = <&UserId>::try_from(user_id).map_err(|_| InvalidInput)?;
        if expected_fingerprint.len() != 64
            || !expected_fingerprint
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(InvalidInput);
        }
        let account = self
            .store
            .get_account(account_id)
            .await
            .map_err(|_| Internal)?
            .ok_or(NotFound)?;
        let lock = lock_for(&self.locks, &account.user_id, &account.homeserver_url);
        let _guard = lock.lock().await;
        let account = self
            .store
            .get_account(account_id)
            .await
            .map_err(|_| Internal)?
            .ok_or(NotFound)?;
        if account.state != AccountState::Active {
            return Err(NotReady);
        }
        let client = self.manager.cached(account_id).ok_or(NotReady)?;
        if client.user_id() == Some(user_id) {
            return Err(InvalidInput);
        }
        let rooms = self
            .manager
            .room_identity_changes()
            .warning_rooms(account_id, user_id);
        let (room, identity) = tokio::time::timeout(LOCAL_READ_TIMEOUT, async {
            for room in rooms.iter().filter_map(|room_id| client.get_room(room_id)) {
                if eligible(&room) && active_member(&room, user_id).await.map_err(|_| Internal)? {
                    let identity = room.user_identity(user_id).await.ok_or(NotFound)?;
                    return Ok((room, identity));
                }
            }
            Err(Conflict)
        })
        .await
        .map_err(|_| NotReady)??;
        resolve_current_warning(account_id, &room, &identity, expected_fingerprint, kind).await
    }
}

async fn resolve_current_warning(
    account_id: Uuid,
    room: &Room,
    identity: &UserIdentity,
    expected_fingerprint: &str,
    kind: IdentityChangeKind,
) -> Result<(), RoomIdentityError> {
    let Some((current_kind, fingerprint)) = current_warning(room, identity) else {
        return Err(RoomIdentityError::Conflict);
    };
    if current_kind != kind || fingerprint != expected_fingerprint {
        return Err(RoomIdentityError::Conflict);
    }

    // These SDK methods log the public master key. Keep the action log under
    // Axon's own target, with the account, user and warning kind only.
    let user_id = identity.user_id();
    async {
        match kind {
            IdentityChangeKind::PinViolation => identity.pin().await,
            IdentityChangeKind::VerificationViolation => identity.withdraw_verification().await,
        }
    }
    .with_subscriber(tracing::Dispatch::none())
    .await
    .map_err(|_| {
        tracing::warn!(%account_id, %user_id, %kind, "room member identity acknowledgement failed");
        RoomIdentityError::Internal
    })?;
    tracing::info!(%account_id, %user_id, %kind, "room member identity change acknowledged");
    Ok(())
}

fn warning_kind(state: &IdentityState) -> Option<IdentityChangeKind> {
    match state {
        IdentityState::PinViolation => Some(IdentityChangeKind::PinViolation),
        IdentityState::VerificationViolation => Some(IdentityChangeKind::VerificationViolation),
        IdentityState::Verified | IdentityState::Pinned => None,
    }
}

fn current_warning(room: &Room, identity: &UserIdentity) -> Option<(IdentityChangeKind, String)> {
    let UserIdentity::Other(other) = identity else {
        return None;
    };
    let kind = warning_kind(&room.state_of(identity))?;
    let key = other.master_key().get_first_key()?;
    let fingerprint = format!("{:x}", Sha256::digest(key.as_bytes()));
    Some((kind, fingerprint))
}

async fn active_member(room: &Room, user_id: &UserId) -> matrix_sdk::Result<bool> {
    Ok(room
        .get_member_no_sync(user_id)
        .await?
        .is_some_and(|member| {
            matches!(
                member.membership(),
                MembershipState::Join | MembershipState::Invite
            )
        }))
}

// The SDK's incremental identity updates can include a user with a cached
// leave or ban event. Only joined or invited members carry room warnings.
async fn active_changes(
    room: &Room,
    mut batch: Vec<IdentityStatusChange>,
) -> matrix_sdk::Result<Vec<IdentityStatusChange>> {
    for change in &mut batch {
        if warning_kind(&change.changed_to).is_some()
            && !active_member(room, &change.user_id).await?
        {
            change.changed_to = IdentityState::Pinned;
        }
    }
    Ok(batch)
}

fn eligible(room: &Room) -> bool {
    room.state() == RoomState::Joined && room.encryption_state().is_encrypted()
}

#[derive(Clone)]
struct AccountWatch {
    account_id: Uuid,
    changes: RoomIdentityChanges,
    details: RoomDetailsChanges,
    gate: Arc<HomeserverGate>,
    /// Limit concurrent room bootstrap work even when no HTTP is needed.
    initializing: Arc<Semaphore>,
}

pub(crate) async fn watch(
    client: Client,
    account_id: Uuid,
    changes: RoomIdentityChanges,
    details: RoomDetailsChanges,
    gate: Arc<HomeserverGate>,
    cancel: CancellationToken,
) {
    let watcher = AccountWatch {
        account_id,
        changes,
        details,
        gate,
        initializing: Arc::new(Semaphore::new(1)),
    };
    let mut observed = HashMap::<OwnedRoomId, (CancellationToken, tokio::task::Id)>::new();
    let mut workers = JoinSet::new();
    let mut tick = tokio::time::interval(RESCAN_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut updates = client.subscribe_to_all_room_updates();
    // Room streams report warning-kind transitions. The account stream also
    // wakes clients when a second rotation requires a new fingerprint.
    let identities = client.encryption().user_identities_stream().await.ok();
    let mut identities = std::pin::pin!(futures_util::stream::iter(identities).flatten());
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            Some(result) = workers.join_next(), if !workers.is_empty() => {
                match result {
                    Ok(room_id) => { observed.remove(&room_id); }
                    Err(error) => {
                        let id = error.id();
                        observed.retain(|room_id, (_, worker_id)| {
                            if *worker_id == id {
                                watcher.forget(room_id);
                                return false;
                            }
                            true
                        });
                        tracing::warn!(%account_id, %error, "room identity watcher stopped unexpectedly");
                    }
                }
            }
            _ = tick.tick() => {
                for room in client.rooms() {
                    watcher.observe(room, &cancel, &mut observed, &mut workers);
                }
            }
            Some(updates) = identities.next() => watcher.invalidate_fingerprints(&updates),
            update = updates.recv() => match update {
                Ok(update) => {
                    for room in update.iter_all_room_ids().filter_map(|id| client.get_room(id)) {
                        watcher.observe(room, &cancel, &mut observed, &mut workers);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => tick.reset_immediately(),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    workers.shutdown().await;
    let rooms = watcher.changes.lock().remove(&account_id);
    if let Some(rooms) = rooms {
        for room_id in rooms.keys() {
            watcher
                .details
                .mark(room_id.as_str(), RoomDetailSource::IdentityChanges);
        }
    }
}

impl AccountWatch {
    fn observe(
        &self,
        room: Room,
        cancel: &CancellationToken,
        observed: &mut HashMap<OwnedRoomId, (CancellationToken, tokio::task::Id)>,
        workers: &mut JoinSet<OwnedRoomId>,
    ) {
        let room_id = room.room_id().to_owned();
        if !eligible(&room) {
            if let Some((token, _)) = observed.get(&room_id) {
                token.cancel();
            }
            return;
        }
        observed.entry(room_id.clone()).or_insert_with(|| {
            let token = cancel.child_token();
            let watch_token = token.clone();
            let watcher = self.clone();
            let task = workers.spawn(async move {
                watch_token.run_until_cancelled(watcher.follow(&room)).await;
                watcher.forget(&room_id);
                room_id
            });
            (token, task.id())
        });
    }

    fn invalidate_fingerprints(&self, updates: &IdentityUpdates) {
        let changed_rooms: Vec<_> = self
            .changes
            .lock()
            .get(&self.account_id)
            .into_iter()
            .flatten()
            .filter(|(_, warnings)| {
                warnings
                    .keys()
                    .any(|user_id| updates.changed.contains_key(user_id))
            })
            .map(|(room_id, _)| room_id.clone())
            .collect();
        for room_id in changed_rooms {
            self.details
                .mark(room_id.as_str(), RoomDetailSource::IdentityChanges);
        }
    }

    async fn follow(&self, room: &Room) {
        let account_id = self.account_id;
        let room_id = room.room_id();
        let mut info = room.subscribe_info();
        while eligible(room) {
            let Some(stream) = self.subscribe(room).await else {
                tracing::debug!(%account_id, %room_id, "room identity watcher will retry");
                self.forget(room_id);
                tokio::time::sleep(RESCAN_INTERVAL).await;
                continue;
            };
            let mut stream = std::pin::pin!(stream);
            // The SDK yields a non-empty starting snapshot before awaiting any
            // updates, and omits the first item entirely for a clean room.
            let initial = stream.next().now_or_never().flatten().unwrap_or_default();
            self.publish(room_id, initial, true);

            // The SDK stream logs and swallows broadcast lag. Periodically
            // replace its snapshot from the durable SDK state to heal that gap.
            let refresh = tokio::time::sleep(RESUBSCRIBE_INTERVAL);
            tokio::pin!(refresh);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut refresh => break,
                    update = info.next() => {
                        if update.is_none() || !eligible(room) {
                            return;
                        }
                        // A limited sync invalidated the roster. Rebuild it through
                        // the gated path before polling the SDK stream again.
                        if !room.are_members_synced() {
                            break;
                        }
                    }
                    batch = stream.next() => match batch {
                        Some(batch) => {
                            let result = tokio::time::timeout(LOCAL_READ_TIMEOUT, active_changes(room, batch)).await;
                            match result {
                                Ok(Ok(batch)) => self.publish(room_id, batch, false),
                                _ => break,
                            }
                        }
                        None => return,
                    },
                }
            }
        }
    }

    async fn subscribe(
        &self,
        room: &Room,
    ) -> Option<impl futures_util::Stream<Item = Vec<IdentityStatusChange>>> {
        let _initializing = self.initializing.acquire().await.ok()?;
        if !eligible(room) {
            return None;
        }
        // Prime the roster explicitly, so failures are visible and the SDK's
        // initial room subscription normally does local work only.
        if !room.are_members_synced() {
            let permit = self
                .gate
                .acquire_briefly(MEMBER_SYNC_TIMEOUT, MEMBER_SYNC_ADMISSION_WAIT)
                .await?;
            // An independent SDK operation can hydrate while we wait for the
            // gate. Drop that unused permit without recording a network success.
            if !room.are_members_synced() {
                let result = tokio::time::timeout(MEMBER_SYNC_TIMEOUT, room.sync_members()).await;
                permit.finish(outcome_of(&result));
                result.ok()?.ok()?;
            }
        }
        if !room.are_members_synced() {
            return None;
        }
        tokio::time::timeout(
            MEMBER_SYNC_TIMEOUT,
            room.subscribe_to_identity_status_changes(),
        )
        .await
        .ok()?
        .ok()
    }

    fn forget(&self, room_id: &RoomId) {
        let removed = self
            .changes
            .lock()
            .get_mut(&self.account_id)
            .and_then(|rooms| rooms.remove(room_id));
        if removed.is_some() {
            self.details
                .mark(room_id.as_str(), RoomDetailSource::IdentityChanges);
        }
    }

    fn publish(&self, room_id: &RoomId, batch: Vec<IdentityStatusChange>, replace: bool) {
        let account_id = self.account_id;
        {
            let mut accounts = self.changes.lock();
            let rooms = accounts.entry(account_id).or_default();
            let old = rooms.get(room_id);
            let mut next = if replace {
                Warnings::new()
            } else {
                old.cloned().unwrap_or_default()
            };
            for change in batch {
                if let Some(kind) = warning_kind(&change.changed_to) {
                    next.insert(change.user_id, kind);
                } else {
                    next.remove(&change.user_id);
                }
            }
            if old == Some(&next) {
                return;
            }
            for (user_id, kind) in &next {
                if old.and_then(|old| old.get(user_id)) != Some(kind) {
                    tracing::info!(%account_id, %room_id, %user_id, %kind, "room member identity warning raised");
                }
            }
            for (user_id, kind) in old.into_iter().flatten() {
                if !next.contains_key(user_id) {
                    tracing::info!(%account_id, %room_id, %user_id, %kind, "room member identity warning cleared");
                }
            }
            rooms.insert(room_id.to_owned(), next);
        }
        self.details
            .mark(room_id.as_str(), RoomDetailSource::IdentityChanges);
    }
}

#[cfg(test)]
mod tests;
