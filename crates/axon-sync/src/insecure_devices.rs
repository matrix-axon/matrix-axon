//! Preflight the MSC4153 setting against the SDK's room and crypto stores.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axon_core::insecure_devices::{InsecureDeviceImpact, InsecureDeviceRoom};
use futures_util::{stream, StreamExt, TryStreamExt};
use matrix_sdk::ruma::events::room::history_visibility::HistoryVisibility;
use matrix_sdk::ruma::{OwnedRoomId, OwnedUserId};
use matrix_sdk::{Client, Room, RoomMemberships};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::gateway::fresh_room_encryption;
use crate::homeserver_gate::{outcome_of, HomeserverGate};
use crate::LifecycleError;

pub(crate) const IMPACT_TIMEOUT: Duration = Duration::from_secs(30);
const MEMBER_SYNC_TIMEOUT: Duration = Duration::from_secs(8);
const MEMBER_SYNC_ADMISSION_WAIT: Duration = Duration::from_secs(2);
const ROOM_READ_CONCURRENCY: usize = 4;
const DEVICE_READ_CONCURRENCY: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum InsecureDevicesError {
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
    #[error("Axon's device must be cross-signed before enabling this setting")]
    OwnDeviceNotCrossSigned,
    #[error("Axon's own cross-signing identity must be verified before enabling this setting")]
    OwnIdentityNotVerified,
    #[error("could not read Axon's own device and cross-signing identity")]
    OwnTrustUnavailable,
    #[error("account operations are still using the current Matrix client; retry shortly")]
    ClientBusy,
    #[error("the previous Matrix client is still closing; retry shortly")]
    ClientRetiring,
    #[error("the current impact must be acknowledged before enabling this setting")]
    AcknowledgementRequired(InsecureDeviceImpact),
    #[error("the impact could not be read; retry when the account has synced")]
    ImpactUnavailable,
}

/// Read the SDK's local cross-signing state instead of the persisted
/// verification flag, which can lag after an identity change.
pub(crate) async fn require_cross_signed_device(
    account_id: Uuid,
    client: &Client,
    affected_rooms: Option<usize>,
) -> Result<(), InsecureDevicesError> {
    use InsecureDevicesError::{
        OwnDeviceNotCrossSigned, OwnIdentityNotVerified, OwnTrustUnavailable,
    };

    let encryption = client.encryption();
    let Some(user) = client.user_id() else {
        return Err(OwnTrustUnavailable);
    };
    let observation = tokio::time::timeout(Duration::from_secs(5), async {
        let identity = encryption.get_user_identity(user).await?;
        let cross_signed = encryption
            .get_own_device()
            .await?
            .is_some_and(|device| device.is_cross_signed_by_owner());
        Ok::<_, matrix_sdk::encryption::CryptoStoreError>((identity, cross_signed))
    })
    .await
    .map_err(|_| OwnTrustUnavailable)?
    .map_err(|error| {
        tracing::warn!(%account_id, %error, "could not read Axon's device trust for MSC4153");
        OwnTrustUnavailable
    })?;
    // IdentityBasedStrategy also refuses sends if the own identity exists but
    // is unverified locally, even when the current device is cross-signed.
    let (reason, error) = match observation {
        (None, _) => ("cross_signing_not_set_up", OwnDeviceNotCrossSigned),
        (_, false) => ("own_device_not_cross_signed", OwnDeviceNotCrossSigned),
        (Some(identity), true) if !identity.is_verified() => {
            ("own_identity_not_verified", OwnIdentityNotVerified)
        }
        (Some(_), true) => return Ok(()),
    };
    tracing::info!(%account_id, affected_rooms, reason, "refused MSC4153 setting change");
    Err(error)
}

/// Each affected user is looked up once, even across rooms. Device lists stay
/// local to the crypto store, so the answer describes what Axon currently knows.
/// A lazy room roster must be hydrated before an impact can be acknowledged.
pub(crate) async fn read_impact(
    account_id: Uuid,
    client: &Client,
    gate: &Arc<HomeserverGate>,
) -> Result<InsecureDeviceImpact, InsecureDevicesError> {
    let rooms: BTreeMap<_, _> = stream::iter(client.joined_rooms())
        .map(|room| read_encrypted_roster(account_id, client, gate, room))
        .buffer_unordered(ROOM_READ_CONCURRENCY)
        .try_filter_map(futures_util::future::ok)
        .try_collect()
        .await?;

    let users: BTreeSet<_> = rooms.values().flatten().cloned().collect();
    let encryption = &client.encryption();
    let unsigned: HashSet<_> = stream::iter(users)
        .map(|user| async move {
            let devices = encryption.get_user_devices(&user).await.map_err(|error| {
                tracing::warn!(%account_id, user_id = %user, %error, "could not read devices for MSC4153 impact");
                InsecureDevicesError::ImpactUnavailable
            })?;
            let affected = devices.devices().any(|device| {
                !device.is_deleted() && !device.is_cross_signed_by_owner()
            });
            Ok::<_, InsecureDevicesError>(affected.then_some(user))
        })
        .buffer_unordered(DEVICE_READ_CONCURRENCY)
        .try_filter_map(futures_util::future::ok)
        .try_collect()
        .await?;

    let rooms = rooms
        .iter()
        .filter_map(|(room_id, users)| {
            let member_user_ids: Vec<_> = users
                .iter()
                .filter(|user| unsigned.contains(*user))
                .map(ToString::to_string)
                .collect();
            (!member_user_ids.is_empty()).then(|| InsecureDeviceRoom {
                room_id: room_id.to_string(),
                member_user_ids,
            })
        })
        .collect::<Vec<_>>();

    let acknowledgement_token = acknowledgement_token(account_id, &rooms);
    Ok(InsecureDeviceImpact {
        rooms,
        acknowledgement_token,
    })
}

async fn read_encrypted_roster(
    account_id: Uuid,
    client: &Client,
    gate: &Arc<HomeserverGate>,
    room: Room,
) -> Result<Option<(OwnedRoomId, BTreeSet<OwnedUserId>)>, InsecureDevicesError> {
    let room_id = room.room_id().to_owned();
    let encryption_state = room.encryption_state();
    if encryption_state.is_unknown() {
        tracing::debug!(%account_id, %room_id, "room encryption unavailable for MSC4153 impact");
        return Err(InsecureDevicesError::ImpactUnavailable);
    }
    if !encryption_state.is_encrypted() {
        let permit = gate
            .acquire_briefly(MEMBER_SYNC_TIMEOUT, MEMBER_SYNC_ADMISSION_WAIT)
            .await
            .ok_or(InsecureDevicesError::ImpactUnavailable)?;
        if room.encryption_settings().is_none() {
            let result = tokio::time::timeout(
                MEMBER_SYNC_TIMEOUT,
                fresh_room_encryption(&room, account_id),
            )
            .await;
            permit.finish(outcome_of(&result));
            if !result
                .map_err(|_| InsecureDevicesError::ImpactUnavailable)?
                .map_err(|error| {
                    tracing::debug!(%account_id, %room_id, %error, "could not confirm room encryption for MSC4153");
                    InsecureDevicesError::ImpactUnavailable
                })?
            {
                return Ok(None);
            }
        }
    }
    if !gate
        .ensure_room_members(&room, MEMBER_SYNC_TIMEOUT, MEMBER_SYNC_ADMISSION_WAIT)
        .await
    {
        tracing::debug!(%account_id, %room_id, "room roster unavailable for MSC4153 impact");
        return Err(InsecureDevicesError::ImpactUnavailable);
    }

    // Same recipient membership filter as the SDK's share_room_key.
    let memberships = if room.history_visibility_or_default() == HistoryVisibility::Joined {
        RoomMemberships::JOIN
    } else {
        RoomMemberships::ACTIVE
    };
    let users = client
        .state_store()
        .get_user_ids(&room_id, memberships)
        .await
        .map_err(|error| {
            tracing::warn!(%account_id, %room_id, %error, "could not read room roster for MSC4153 impact");
            InsecureDevicesError::ImpactUnavailable
        })?;
    Ok(Some((room_id, users.into_iter().collect())))
}

fn acknowledgement_token(account_id: Uuid, rooms: &[InsecureDeviceRoom]) -> String {
    // Room and user IDs come from ordered collections. Frame the entries so
    // rearranging members across rooms cannot reproduce the same input bytes.
    let mut digest = Sha256::new();
    digest.update(b"axon-msc4153-impact-v1");
    digest.update(account_id.as_bytes());
    for room in rooms {
        digest.update((room.room_id.len() as u64).to_be_bytes());
        digest.update(room.room_id.as_bytes());
        digest.update((room.member_user_ids.len() as u64).to_be_bytes());
        for user in &room.member_user_ids {
            digest.update((user.len() as u64).to_be_bytes());
            digest.update(user.as_bytes());
        }
    }
    format!("{:x}", digest.finalize())
}
