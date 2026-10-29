//! Sender-device trust evaluation: the per-event verification bundle (M7c).
//!
//! Part A persists a four-valued *snapshot* of the sending device's trust at
//! decrypt time. This module reads that snapshot back and pairs it with **live**
//! cross-signing evidence — the sender's device and identity as the SDK sees them
//! *now* — for the `GET …/events/{id}/verification` endpoint. The two are kept
//! deliberately separate (ADR 0031): the snapshot is immutable (what Matrix's
//! evidence said at receipt); the live evidence can differ (a device trusted when
//! it sent can later be revoked, and vice versa).
//!
//! Mirrors the [`VerificationEngine`](crate::verification::VerificationEngine)
//! shape: a concrete engine owned by [`SyncEngine`](crate::SyncEngine), adapted
//! onto the API's `SenderTrustService` port by `axon-server`. Unlike the verify
//! verbs it is **read-only** and takes no per-identity lock — see
//! [`SenderTrustEngine::bundle`] for why taking one would make a per-message read
//! endpoint wait behind lifecycle verbs that talk to the homeserver.

use axon_store::{AccountState, EventSenderTrust, Store};
use matrix_sdk::ruma::{OwnedDeviceId, OwnedUserId};
use matrix_sdk::Client;
use uuid::Uuid;

use crate::error::GatewayError;
use crate::manager::ClientManager;

/// What can go wrong assembling a trust bundle. The adapter in `axon-server` maps
/// these onto `axon-api`'s HTTP-shaped `TrustError`.
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    /// No account exists for the given id.
    #[error("no such account: {0}")]
    AccountNotFound(Uuid),
    /// No event exists for the account.
    #[error("no such event: {0}")]
    EventNotFound(String),
    /// The account is logged out (`deactivated`) or mid-teardown (`deleting`) — no
    /// live client to read from. (Both surface as `AccountNotActive` from the cold-
    /// connect gate.)
    #[error("account is not active: {0}")]
    NotActive(Uuid),
    /// The upstream homeserver / SDK failed reading the sender's keys.
    #[error("upstream error: {0}")]
    Upstream(String),
    /// A store failure.
    #[error("store error: {0}")]
    Store(String),
}

/// The at-decrypt snapshot half of a [`TrustBundle`] — present only when the
/// event carried a crypto sibling row.
#[derive(Debug, Clone)]
pub struct TrustSnapshot {
    pub sender_trust: Option<String>,
    pub verification_state: Option<String>,
    pub device_id: Option<String>,
    pub curve25519_key: Option<String>,
    pub ed25519_key: Option<String>,
    /// Megolm session provenance recorded at decrypt time (the spec's
    /// content-authentication evidence, ADR 0031): the session id and whether
    /// the key reached us forwarded, with the forwarder's identity if so.
    pub session_id: Option<String>,
    pub forwarded: Option<bool>,
    pub forwarder_user_id: Option<String>,
    pub forwarder_device_id: Option<String>,
}

/// The live evidence half of a [`TrustBundle`], read from the SDK at request time.
#[derive(Debug, Clone)]
pub struct CurrentTrust {
    pub device_known: bool,
    pub device_cross_signed: Option<bool>,
    pub identity_known: bool,
    pub identity_verified: Option<bool>,
    pub verification_violation: Option<bool>,
    pub previously_verified: Option<bool>,
    pub master_key: Option<String>,
}

/// A per-event trust bundle: the durable at-decrypt snapshot plus live evidence.
#[derive(Debug, Clone)]
pub struct TrustBundle {
    pub sender: String,
    pub snapshot: Option<TrustSnapshot>,
    pub current: CurrentTrust,
}

/// Evaluates per-event sender-device trust. Cheap to clone (handles only). Built
/// by [`SyncEngine::sender_trust`](crate::SyncEngine::sender_trust) and adapted
/// onto the API's sender-trust port by `axon-server`.
#[derive(Clone)]
pub struct SenderTrustEngine {
    store: Store,
    manager: ClientManager,
}

impl SenderTrustEngine {
    pub(crate) fn new(store: Store, manager: ClientManager) -> Self {
        Self { store, manager }
    }

    /// Assemble the trust bundle for `(account_id, event_id)`.
    ///
    /// Unlike the verification verbs, this is **read-only** and takes no
    /// per-identity lifecycle lock: a bundle read tolerates a client a concurrent
    /// logout/delete is severing (worst case an `Upstream` error), and login and
    /// the verify verbs hold that lock across homeserver calls, so taking it here
    /// would make a read wait out a slow homeserver it never talks to. The live
    /// evidence is read from the local crypto store (see
    /// [`current_trust`](Self::current_trust)), so this awaits no homeserver
    /// request and needs no request timeout (issue #57). The `active` gate
    /// is an explicit store read (see below) rather than
    /// [`ClientManager::get_or_connect`]'s, whose cache-hit fast path can return a
    /// still-cached client for a just-deactivated account without re-checking the
    /// row.
    pub async fn bundle(
        &self,
        account_id: Uuid,
        event_id: &str,
    ) -> Result<TrustBundle, TrustError> {
        // The snapshot read also resolves event existence (→ 404) and the sender.
        let snap = self
            .store
            .event_sender_trust(account_id, event_id)
            .await
            .map_err(|e| TrustError::Store(e.to_string()))?
            .ok_or_else(|| TrustError::EventNotFound(event_id.to_owned()))?;

        // Explicit active-state gate, *not* `get_or_connect`'s. `get_or_connect`
        // re-checks `accounts.state` only on a cache miss — its cache-hit fast path
        // returns a still-cached client without re-reading the row. During logout
        // the row flips to `deactivated` *before* the task drains and the slot is
        // emptied (and a drain that fails can leave the client cached indefinitely),
        // so relying on the connect gate alone could serve live trust for a
        // deactivated account with a `200` despite the documented `409`. Read the
        // authoritative state from the store first so a cached-client hit can't
        // bypass it. (A benign race remains — the row could flip *after* this read —
        // but that window is the same one every read endpoint has, not the
        // indefinitely-stale cached-client hole.)
        let account = self
            .store
            .get_account(account_id)
            .await
            .map_err(|e| TrustError::Store(e.to_string()))?
            .ok_or(TrustError::AccountNotFound(account_id))?;
        if account.state != AccountState::Active {
            return Err(TrustError::NotActive(account_id));
        }

        // `get_or_connect` re-validates existence (`UnknownAccount` → 404) and
        // `active` (`AccountNotActive` → 409) on a cold connect; after the explicit
        // gate above it primarily yields the live client to read evidence from.
        let client = self
            .manager
            .get_or_connect(account_id)
            .await
            .map_err(|e| match e {
                GatewayError::UnknownAccount(id) => TrustError::AccountNotFound(id),
                GatewayError::AccountNotActive(id) => TrustError::NotActive(id),
                other => TrustError::Upstream(other.to_string()),
            })?;

        let sender: OwnedUserId = snap.sender.parse().map_err(|_| {
            TrustError::Upstream(format!("invalid sender user id: {}", snap.sender))
        })?;

        let current = self
            .current_trust(&client, &sender, snap.device_id.as_deref())
            .await?;
        Ok(TrustBundle {
            sender: snap.sender.clone(),
            snapshot: snapshot(&snap),
            current,
        })
    }

    /// Read the sender's current device + identity trust from the live SDK. A
    /// missing device or identity (deleted, not yet downloaded, federation lag) is
    /// reported as "unknown" rather than an error — it's a normal condition.
    ///
    /// Both lookups are local crypto-store reads. The SDK's `get_device` and
    /// `get_user_identity` pass no key-query timeout to the `OlmMachine`, which
    /// then returns what is stored without waiting for a pending `/keys/query`
    /// (matrix-sdk 0.19). So "current" means as of the sync loop's last key
    /// download, and a stale device list reads stale rather than blocking.
    async fn current_trust(
        &self,
        client: &Client,
        sender: &OwnedUserId,
        device_id: Option<&str>,
    ) -> Result<CurrentTrust, TrustError> {
        let encryption = client.encryption();

        // Known limitation (issue #59): we look the device up under the event's
        // `sender`. For a `MismatchedSender` event the session device belongs to a
        // *different* user than `sender`, so `get_device` returns `None` here and
        // `device_known` is reported `false` — the live half can't surface a
        // mismatched device. The at-decrypt `sender_trust` snapshot still records
        // the mismatch, so it isn't lost, just not re-derivable live.
        let (device_known, device_cross_signed) = match device_id {
            Some(device_id) => {
                let device_id: OwnedDeviceId = device_id.into();
                match encryption
                    .get_device(sender, &device_id)
                    .await
                    .map_err(|e| TrustError::Upstream(e.to_string()))?
                {
                    Some(device) => (true, Some(device.is_cross_signed_by_owner())),
                    None => (false, None),
                }
            }
            None => (false, None),
        };

        let identity = encryption
            .get_user_identity(sender)
            .await
            .map_err(|e| TrustError::Upstream(e.to_string()))?;
        let (
            identity_known,
            identity_verified,
            verification_violation,
            previously_verified,
            master_key,
        ) = match identity {
            Some(identity) => (
                true,
                Some(identity.is_verified()),
                Some(identity.has_verification_violation()),
                Some(identity.was_previously_verified()),
                identity.master_key().get_first_key().map(|k| k.to_base64()),
            ),
            None => (false, None, None, None, None),
        };

        Ok(CurrentTrust {
            device_known,
            device_cross_signed,
            identity_known,
            identity_verified,
            verification_violation,
            previously_verified,
            master_key,
        })
    }
}

/// Build the snapshot half from the stored row — `None` when no crypto sibling
/// was recorded (`verification_state` is `NOT NULL` in the sibling, so its
/// presence is the signal a snapshot exists).
fn snapshot(snap: &EventSenderTrust) -> Option<TrustSnapshot> {
    snap.verification_state.as_ref()?;
    Some(TrustSnapshot {
        sender_trust: snap.sender_trust.clone(),
        verification_state: snap.verification_state.clone(),
        device_id: snap.device_id.clone(),
        curve25519_key: snap.curve25519_key.clone(),
        ed25519_key: snap.ed25519_key.clone(),
        session_id: snap.session_id.clone(),
        forwarded: snap.forwarded,
        forwarder_user_id: snap.forwarder_user_id.clone(),
        forwarder_device_id: snap.forwarder_device_id.clone(),
    })
}
