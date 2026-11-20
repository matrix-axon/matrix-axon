//! Own cross-signing identity monitoring (ADR 0136).

use std::time::Duration;

use axon_core::{LiveFrame, OwnIdentityFrame};
use axon_store::{Store, StoreError};
use futures_util::StreamExt;
use matrix_sdk::Client;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::lifecycle::{derive_verified, IdentityLock};

#[cfg(test)]
mod tests;

const RECHECK_INTERVAL: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct Observation {
    master_key: Option<[u8; 32]>,
    pub verified: bool,
}

async fn master_key(client: &Client, account_id: Uuid) -> Option<[u8; 32]> {
    let user_id = client.user_id()?;
    match client.encryption().get_user_identity(user_id).await {
        Ok(identity) => identity
            .and_then(|identity| identity.master_key().get_first_key())
            .map(|key| *key.as_bytes()),
        Err(_) => {
            tracing::warn!(%account_id, %user_id, "failed to read own cross-signing identity");
            None
        }
    }
}

/// Pair the master key with the SDK's own-device check. The CryptoStore can
/// change during this read, independently of the Axon lifecycle lock. If two
/// reads straddle a rotation, keep the stored baseline and the pending alert.
pub(crate) async fn observe(client: &Client, account_id: Uuid) -> Observation {
    let before = master_key(client, account_id).await;
    let verified = derive_verified(client).await;
    let after = master_key(client, account_id).await;
    Observation {
        master_key: before.filter(|key| Some(*key) == after),
        verified,
    }
}

/// The caller holds the account's lifecycle lock across observation, commit
/// and publication. Reconnecting clients recover the pending alert from GET.
pub(crate) async fn persist(
    client: &Client,
    store: &Store,
    account_id: Uuid,
    observation: &Observation,
    live_tx: &broadcast::Sender<LiveFrame>,
) -> Result<(), StoreError> {
    let update = store
        .record_account_verification(
            account_id,
            observation.verified,
            observation.master_key.as_ref(),
        )
        .await?;
    if let Some(update) = update.filter(|update| {
        update.identity_replaced || update.own_identity_changed != update.was_identity_changed
    }) {
        tracing::info!(
            %account_id,
            user_id = ?client.user_id(),
            own_identity_changed = update.own_identity_changed,
            identity_replaced = update.identity_replaced,
            "account cross-signing identity status changed"
        );
        let _ = live_tx.send(LiveFrame::OwnIdentityChanged(OwnIdentityFrame {
            account_id,
            own_identity_changed: update.own_identity_changed,
            identity_replaced: update.identity_replaced,
        }));
    }
    Ok(())
}

pub(crate) async fn lock_and_persist(
    lock: &IdentityLock,
    client: &Client,
    store: &Store,
    account_id: Uuid,
    live_tx: &broadcast::Sender<LiveFrame>,
    cancel: &CancellationToken,
) {
    // Logout/delete drain this watcher while holding the same lock.
    let _guard = tokio::select! {
        biased;
        _ = cancel.cancelled() => return,
        guard = lock.lock() => guard,
    };
    let observation = match tokio::time::timeout(READ_TIMEOUT, observe(client, account_id)).await {
        Ok(observation) => observation,
        Err(_) => {
            tracing::warn!(%account_id, "timed out reading account verification state");
            return;
        }
    };
    if let Err(err) = persist(client, store, account_id, &observation, live_tx).await {
        tracing::warn!(%account_id, error = %err, "failed to persist account verification state");
    }
}

pub(crate) async fn watch(
    client: Client,
    store: Store,
    account_id: Uuid,
    lock: IdentityLock,
    live_tx: broadcast::Sender<LiveFrame>,
    cancel: CancellationToken,
) {
    let Some(user_id) = client.user_id() else {
        return;
    };
    let encryption = client.encryption();
    let mut verification = encryption.verification_state();
    let subscribe = || async {
        match encryption.user_identities_stream().await {
            Ok(stream) => Some(Box::pin(stream)),
            Err(_) => {
                tracing::debug!(%account_id, "own identity subscription will retry");
                None
            }
        }
    };
    let mut identities = tokio::select! {
        _ = cancel.cancelled() => return,
        stream = subscribe() => stream,
    };
    lock_and_persist(&lock, &client, &store, account_id, &live_tx, &cancel).await;
    // Protect against failed subscriptions, missed SDK updates and failed writes.
    let mut recheck = tokio::time::interval_at(
        tokio::time::Instant::now() + RECHECK_INTERVAL,
        RECHECK_INTERVAL,
    );
    recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            Some(_) = verification.next() => {}
            _ = recheck.tick() => {
                if identities.is_none() {
                    identities = tokio::select! {
                        _ = cancel.cancelled() => return,
                        stream = subscribe() => stream,
                    };
                }
            }
            next = async {
                match &mut identities {
                    Some(stream) => stream.next().await,
                    None => std::future::pending().await,
                }
            } => match next {
                Some(update) if update.new.contains_key(user_id)
                    || update.changed.contains_key(user_id) => {}
                Some(_) => continue,
                None => {
                    identities = None;
                    continue;
                }
            }
        }
        lock_and_persist(&lock, &client, &store, account_id, &live_tx, &cancel).await;
    }
}
