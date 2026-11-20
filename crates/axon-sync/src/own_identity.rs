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
    identity: Option<Identity>,
    pub verified: bool,
}

#[derive(PartialEq, Eq)]
struct Identity {
    master_key: [u8; 32],
    verification_violation: bool,
}

async fn identity(client: &Client, account_id: Uuid) -> Option<Identity> {
    let user_id = client.user_id()?;
    match client.encryption().get_user_identity(user_id).await {
        Ok(identity) => identity.and_then(|identity| {
            let key = identity.master_key().get_first_key()?;
            Some(Identity {
                master_key: *key.as_bytes(),
                verification_violation: identity.has_verification_violation(),
            })
        }),
        Err(_) => {
            tracing::debug!(%account_id, "failed to read own cross-signing identity");
            None
        }
    }
}

/// Pair the master key with the SDK's own-device check. The CryptoStore can
/// change during this read, independently of the Axon lifecycle lock. Require
/// two matching observations of both the master key and verification-violation
/// state before the store can set a baseline or change the pending alert.
pub(crate) async fn observe(client: &Client, account_id: Uuid) -> Observation {
    let before = identity(client, account_id).await;
    let verified = derive_verified(client).await;
    let after = identity(client, account_id).await;
    Observation {
        identity: after.filter(|identity| Some(identity) == before.as_ref()),
        verified,
    }
}

async fn read_with_timeout(client: &Client, account_id: Uuid) -> Option<Observation> {
    match tokio::time::timeout(READ_TIMEOUT, observe(client, account_id)).await {
        Ok(observation) => Some(observation),
        Err(_) => {
            tracing::warn!(%account_id, "timed out reading account verification state");
            None
        }
    }
}

/// Recovery reads back immediately. Retry a missing or racing identity once
/// before relying on the watcher to reconcile it.
pub(crate) async fn retry_after_recovery(
    client: &Client,
    account_id: Uuid,
    first: Observation,
) -> Observation {
    if first.identity.is_some() {
        return first;
    }
    read_with_timeout(client, account_id)
        .await
        .filter(|retry| retry.identity.is_some())
        .unwrap_or(first)
}

pub(crate) fn publish_update(
    account_id: Uuid,
    own_identity_changed: bool,
    identity_replaced: bool,
    live_tx: &broadcast::Sender<LiveFrame>,
) {
    tracing::info!(
        %account_id,
        own_identity_changed,
        identity_replaced,
        "account cross-signing identity status changed"
    );
    let _ = live_tx.send(LiveFrame::OwnIdentityChanged(OwnIdentityFrame {
        account_id,
        own_identity_changed,
        identity_replaced,
    }));
}

/// The caller holds the account's lifecycle lock across observation, commit
/// and publication. Reconnecting clients recover the pending alert from GET.
pub(crate) async fn persist(
    store: &Store,
    account_id: Uuid,
    observation: &Observation,
    live_tx: &broadcast::Sender<LiveFrame>,
) -> Result<(), StoreError> {
    let identity = observation.identity.as_ref();
    let update = store
        .record_account_verification(
            account_id,
            observation.verified,
            identity.map(|identity| &identity.master_key),
            identity.is_some_and(|identity| identity.verification_violation),
        )
        .await?;
    if let Some(update) = update.filter(|update| {
        update.identity_replaced || update.own_identity_changed != update.was_identity_changed
    }) {
        publish_update(
            account_id,
            update.own_identity_changed,
            update.identity_replaced,
            live_tx,
        );
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
    let Some(observation) = read_with_timeout(client, account_id).await else {
        return;
    };
    if let Err(err) = persist(store, account_id, &observation, live_tx).await {
        tracing::warn!(%account_id, error = %err, "failed to persist account verification state");
    }
}

async fn needs_recheck(client: &Client, store: &Store, account_id: Uuid) -> bool {
    let Some(observation) = read_with_timeout(client, account_id).await else {
        return false;
    };
    let master_key = observation
        .identity
        .as_ref()
        .map(|identity| &identity.master_key);
    match store
        .account_verification_needs_update(account_id, observation.verified, master_key)
        .await
    {
        Ok(needs_update) => needs_update,
        Err(error) => {
            tracing::warn!(%account_id, %error, "failed to check for stale account verification state");
            false
        }
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
    let user_id = client.user_id();
    if user_id.is_none() {
        tracing::warn!(%account_id, "own identity watcher has no Matrix user ID");
    }
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
                // The SDK stream drops lag errors. Compare with durable state
                // outside the lifecycle lock and take it only for corrections.
                if !tokio::select! {
                    _ = cancel.cancelled() => return,
                    needs_update = needs_recheck(&client, &store, account_id) => needs_update,
                } {
                    continue;
                }
            }
            next = async {
                match &mut identities {
                    Some(stream) => stream.next().await,
                    None => std::future::pending().await,
                }
            } => match next {
                Some(update) if user_id.is_some_and(|user_id|
                    update.new.contains_key(user_id) || update.changed.contains_key(user_id)
                ) => {}
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
