//! Room-member identity warnings from the Matrix SDK (ADR 0136, decision 3).
use std::{
    collections::{BTreeMap, HashMap},
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
    time::Duration,
};

use axon_core::{IdentityChangeKind, RoomDetailSource, RoomIdentityChange};
use axon_store::{AccountState, Store};
use futures_util::{FutureExt, StreamExt};
use matrix_sdk::{
    encryption::identities::{IdentityUpdates, UserIdentity},
    ruma::{events::room::member::MembershipState, OwnedRoomId, OwnedUserId, RoomId, UserId},
    Client, Room, RoomMemberships, RoomState,
};
use matrix_sdk_base::crypto::{IdentityStatusChange, RoomIdentityProvider};
use sha2::{Digest, Sha256};
use tokio::{
    sync::{Notify, Semaphore},
    task::JoinSet,
};
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

const RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Warning {
    kind: IdentityChangeKind,
    fingerprint: String,
}

type Warnings = BTreeMap<OwnedUserId, Warning>;
type Rooms = HashMap<OwnedRoomId, Warnings>;

/// Each supervised account run owns its entries. HTTP readers take snapshots
/// without holding the lock across SDK work.
#[derive(Clone, Default)]
pub(crate) struct RoomIdentityChanges(Arc<Mutex<HashMap<Uuid, Rooms>>>);

impl RoomIdentityChanges {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, Rooms>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        let mut warnings = warnings;
        if let Some(client) = self.manager.cached(account_id) {
            let room = client.get_room(room_id)?;
            if !eligible(&room) {
                return None;
            }
            recheck_room_warnings(account_id, &room, &mut warnings).await;
        }
        Some(
            warnings
                .into_iter()
                .map(|(user_id, warning)| RoomIdentityChange {
                    user_id: user_id.to_string(),
                    kind: warning.kind,
                    fingerprint: warning.fingerprint,
                })
                .collect(),
        )
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
        let (identity, warning) = tokio::time::timeout(LOCAL_READ_TIMEOUT, async {
            for room in rooms.iter().filter_map(|room_id| client.get_room(room_id)) {
                if eligible(&room) && active_member(&room, user_id).await.map_err(|_| Internal)? {
                    return read_identity(&room, user_id)
                        .await
                        .map_err(|_| Internal)?
                        .ok_or(NotFound);
                }
            }
            Err(Conflict)
        })
        .await
        .map_err(|_| NotReady)??;
        resolve_current_warning(
            account_id,
            &identity,
            warning.as_ref(),
            expected_fingerprint,
            kind,
        )
        .await
    }
}

async fn resolve_current_warning(
    account_id: Uuid,
    identity: &UserIdentity,
    warning: Option<&Warning>,
    expected_fingerprint: &str,
    kind: IdentityChangeKind,
) -> Result<(), RoomIdentityError> {
    if !warning
        .is_some_and(|warning| warning.kind == kind && warning.fingerprint == expected_fingerprint)
    {
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

#[derive(Debug, thiserror::Error)]
enum ObservationError {
    #[error("room membership read failed")]
    Membership(#[from] matrix_sdk::Error),
    #[error("crypto store read failed")]
    Crypto(#[from] matrix_sdk_base::crypto::store::CryptoStoreError),
    #[error("identity changed or became unavailable during classification")]
    IdentityUnavailable,
}

// The SDK client identity exposes verification status and the master key. Its
// room provider additionally exposes the pin classifier, but collapses read
// errors into absence. Only the typed read can establish absence. If the
// classifier cannot observe the same identity, the observation failed.
async fn read_identity(
    room: &Room,
    user_id: &UserId,
) -> Result<Option<(UserIdentity, Option<Warning>)>, ObservationError> {
    let Some(identity) = room
        .client()
        .encryption()
        .get_user_identity(user_id)
        .await?
    else {
        return Ok(None);
    };
    let kind = if identity.is_verified() || user_id == room.own_user_id() {
        None
    } else if identity.has_verification_violation() {
        Some(IdentityChangeKind::VerificationViolation)
    } else {
        let Some(matrix_sdk_base::crypto::UserIdentity::Other(other)) =
            room.user_identity(user_id).await
        else {
            return Err(ObservationError::IdentityUnavailable);
        };
        if other.master_key().get_first_key() != identity.master_key().get_first_key()
            || other.is_verified()
            || other.has_verification_violation()
        {
            return Err(ObservationError::IdentityUnavailable);
        }
        other
            .identity_needs_user_approval()
            .then_some(IdentityChangeKind::PinViolation)
    };
    let warning = kind
        .map(|kind| {
            let key = identity
                .master_key()
                .get_first_key()
                .ok_or(ObservationError::IdentityUnavailable)?;
            Ok::<_, ObservationError>(Warning {
                kind,
                fingerprint: format!("{:x}", Sha256::digest(key.as_bytes())),
            })
        })
        .transpose()?;
    Ok(Some((identity, warning)))
}

async fn member_warning(
    room: &Room,
    user_id: &UserId,
) -> Result<Option<Warning>, ObservationError> {
    if !active_member(room, user_id).await? {
        return if room.are_members_synced() {
            Ok(None)
        } else {
            Err(ObservationError::IdentityUnavailable)
        };
    }
    Ok(read_identity(room, user_id)
        .await?
        .and_then(|(_, warning)| warning))
}

async fn recheck_room_warnings(account_id: Uuid, room: &Room, warnings: &mut Warnings) {
    refresh_known_warnings(account_id, room.room_id(), warnings, |user_id| async move {
        member_warning(room, &user_id).await
    })
    .await;
}

async fn refresh_known_warnings<Fut>(
    account_id: Uuid,
    room_id: &RoomId,
    warnings: &mut Warnings,
    mut read: impl FnMut(OwnedUserId) -> Fut,
) where
    Fut: std::future::Future<Output = Result<Option<Warning>, ObservationError>>,
{
    let users: Vec<_> = warnings.keys().cloned().collect();
    let refreshed = tokio::time::timeout(LOCAL_READ_TIMEOUT, async {
        for user_id in users {
            match read(user_id.clone()).await {
                Ok(Some(warning)) => { warnings.insert(user_id, warning); }
                Ok(None) => { warnings.remove(&user_id); }
                Err(error) => tracing::warn!(%account_id, %room_id, %user_id, %error, "room identity warning recheck failed"),
            }
        }
    }).await;
    if refreshed.is_err() {
        tracing::warn!(%account_id, %room_id, "room identity warning recheck timed out");
    }
}

async fn read_snapshot(room: &Room) -> Result<Warnings, ObservationError> {
    let members = room
        .members_no_sync(RoomMemberships::JOIN | RoomMemberships::INVITE)
        .await?;
    let mut warnings = Warnings::new();
    for member in members {
        let user = member.user_id();
        if user != room.own_user_id() {
            if let Some((_, Some(warning))) = read_identity(room, user).await? {
                warnings.insert(user.to_owned(), warning);
            }
        }
    }
    if !room.are_members_synced() {
        return Err(ObservationError::IdentityUnavailable);
    }
    Ok(warnings)
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
    while !cancel.is_cancelled() {
        let mut workers = JoinSet::new();
        let result = AssertUnwindSafe(watcher.watch_account(&client, &cancel, &mut workers))
            .catch_unwind()
            .await;
        workers.shutdown().await;
        if !cancel.is_cancelled() {
            tracing::warn!(%account_id, panicked = result.is_err(), "room identity account watcher stopped; will restart");
            cancel
                .run_until_cancelled(tokio::time::sleep(RETRY_DELAY))
                .await;
        }
    }
    let rooms = watcher.changes.lock().remove(&account_id);
    if let Some(rooms) = rooms {
        for room_id in rooms.keys() {
            watcher
                .details
                .mark(room_id.as_str(), RoomDetailSource::IdentityChanges);
        }
    }
}

struct RoomWatch {
    cancel: CancellationToken,
    task: tokio::task::Id,
    refresh: Arc<Notify>,
}

impl AccountWatch {
    async fn watch_account(
        &self,
        client: &Client,
        cancel: &CancellationToken,
        workers: &mut JoinSet<OwnedRoomId>,
    ) {
        let account_id = self.account_id;
        let mut observed = HashMap::<OwnedRoomId, RoomWatch>::new();
        let mut tick = tokio::time::interval(RESCAN_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut updates = client.subscribe_to_all_room_updates();
        let encryption = client.encryption();
        let mut identities: Option<futures_util::stream::BoxStream<'_, IdentityUpdates>> = None;
        let mut retry = tokio::time::interval(RETRY_DELAY);
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                Some(result) = workers.join_next(), if !workers.is_empty() => {
                    match result {
                        Ok(room_id) => { observed.remove(&room_id); }
                        Err(error) => {
                            observed.retain(|_, watch| watch.task != error.id());
                            tracing::warn!(%account_id, %error, "room identity worker stopped unexpectedly; will retry");
                        }
                    }
                }
                _ = tick.tick() => {
                    for room in client.rooms() {
                        self.observe(room, cancel, &mut observed, workers);
                    }
                }
                _ = retry.tick(), if identities.is_none() => {
                    match tokio::time::timeout(LOCAL_READ_TIMEOUT, encryption.user_identities_stream()).await {
                        Ok(Ok(stream)) => identities = Some(stream.boxed()),
                        _ => tracing::warn!(%account_id, "could not subscribe to account identity updates; will retry"),
                    }
                }
                update = async { identities.as_mut().expect("enabled with an active stream").next().await }, if identities.is_some() => {
                    match update {
                        Some(updates) => self.refresh_changed_rooms(&updates, &observed),
                        None => {
                            tracing::warn!(%account_id, "account identity stream ended; will resubscribe");
                            identities = None;
                            retry.reset_after(RETRY_DELAY);
                        }
                    }
                }
                update = updates.recv() => match update {
                    Ok(update) => {
                        for room in update.iter_all_room_ids().filter_map(|id| client.get_room(id)) {
                            self.observe(room, cancel, &mut observed, workers);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => tick.reset_immediately(),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
            }
        }
    }

    fn observe(
        &self,
        room: Room,
        cancel: &CancellationToken,
        observed: &mut HashMap<OwnedRoomId, RoomWatch>,
        workers: &mut JoinSet<OwnedRoomId>,
    ) {
        let room_id = room.room_id().to_owned();
        if !eligible(&room) {
            if let Some(watch) = observed.get(&room_id) {
                watch.cancel.cancel();
            }
            self.forget(&room_id);
            return;
        }
        observed.entry(room_id.clone()).or_insert_with(|| {
            let cancel = cancel.child_token();
            let refresh = Arc::new(Notify::new());
            let watch_cancel = cancel.clone();
            let watch_refresh = refresh.clone();
            let watcher = self.clone();
            let task = workers.spawn(async move {
                watch_cancel
                    .run_until_cancelled(watcher.follow(&room, &watch_refresh))
                    .await;
                if !eligible(&room) {
                    watcher.forget(&room_id);
                }
                room_id
            });
            RoomWatch {
                cancel,
                task: task.id(),
                refresh,
            }
        });
    }

    // Room streams omit same-kind changes. Wake the affected room workers so
    // they can record the new fingerprint before notifying connected clients.
    fn refresh_changed_rooms(
        &self,
        updates: &IdentityUpdates,
        observed: &HashMap<OwnedRoomId, RoomWatch>,
    ) {
        let accounts = self.changes.lock();
        if let Some(rooms) = accounts.get(&self.account_id) {
            for (room_id, warnings) in rooms {
                if warnings
                    .keys()
                    .any(|id| updates.changed.contains_key(id) || updates.new.contains_key(id))
                {
                    if let Some(watch) = observed.get(room_id) {
                        watch.refresh.notify_one();
                    }
                }
            }
        }
    }

    async fn follow(&self, room: &Room, refresh_known: &Notify) {
        let account_id = self.account_id;
        let room_id = room.room_id();
        let mut info = room.subscribe_info();
        while eligible(room) {
            let Some((stream, snapshot)) = self.subscribe(room).await else {
                tracing::debug!(%account_id, %room_id, "room identity watcher will retry");
                tokio::time::sleep(RESCAN_INTERVAL).await;
                continue;
            };
            let mut stream = std::pin::pin!(stream);
            self.publish(room, snapshot);

            // The SDK logs and hides broadcast lag. Independently re-read the
            // roster and fingerprints to repair any missed updates.
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
                        if !room.are_members_synced() {
                            break;
                        }
                    }
                    _ = refresh_known.notified() => {
                        if let Some(mut warnings) = self.changes.warnings(account_id, room_id) {
                            recheck_room_warnings(account_id, room, &mut warnings).await;
                            self.publish(room, warnings);
                        }
                    }
                    batch = stream.next() => match batch {
                        Some(batch) => {
                            let mut warnings = self.changes.warnings(account_id, room_id).unwrap_or_default();
                            let result = tokio::time::timeout(LOCAL_READ_TIMEOUT, async {
                                for change in batch {
                                    match member_warning(room, &change.user_id).await? {
                                        Some(warning) => { warnings.insert(change.user_id, warning); }
                                        None => { warnings.remove(&change.user_id); }
                                    }
                                }
                                Ok::<_, ObservationError>(())
                            }).await;
                            match result {
                                Ok(Ok(())) => self.publish(room, warnings),
                                _ => break,
                            }
                        }
                        None => {
                            tracing::warn!(%account_id, %room_id, "room identity stream ended; will resubscribe");
                            tokio::time::sleep(RETRY_DELAY).await;
                            break;
                        }
                    },
                }
            }
        }
    }

    async fn subscribe(
        &self,
        room: &Room,
    ) -> Option<(
        impl futures_util::Stream<Item = Vec<IdentityStatusChange>>,
        Warnings,
    )> {
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
        // Subscribe before reading the fallible snapshot so later changes are
        // buffered. The SDK's optional first batch can hide roster and crypto
        // store errors and is never evidence for replacing the cache.
        tokio::time::timeout(MEMBER_SYNC_TIMEOUT, async {
            let stream = room.subscribe_to_identity_status_changes().await?;
            let mut stream = Box::pin(stream);
            let _ = stream.next().now_or_never();
            let snapshot = read_snapshot(room).await?;
            Ok::<_, ObservationError>((stream, snapshot))
        })
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

    fn publish(&self, room: &Room, warnings: Warnings) {
        if !eligible(room) {
            return;
        }
        let account_id = self.account_id;
        let room_id = room.room_id();
        let previous = {
            let mut accounts = self.changes.lock();
            let rooms = accounts.entry(account_id).or_default();
            if rooms.get(room_id) == Some(&warnings) {
                return;
            }
            rooms
                .insert(room_id.to_owned(), warnings.clone())
                .unwrap_or_default()
        };
        for (user_id, warning) in &warnings {
            if previous.get(user_id) != Some(warning) {
                tracing::info!(%account_id, %room_id, %user_id, kind = %warning.kind, "room member identity warning changed");
            }
        }
        for (user_id, warning) in &previous {
            if !warnings.contains_key(user_id) {
                tracing::info!(%account_id, %room_id, %user_id, kind = %warning.kind, "room member identity warning cleared");
            }
        }
        self.details
            .mark(room_id.as_str(), RoomDetailSource::IdentityChanges);
    }
}

#[cfg(test)]
mod tests;
