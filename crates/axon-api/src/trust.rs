//! The sender-trust port the verification-bundle handler depends on (M7c).
//!
//! Like [`VerificationService`](crate::verification::VerificationService), this
//! crate defines the capability it *needs* — assembling a per-event trust bundle
//! from the durable at-decrypt snapshot plus live cross-signing evidence — rather
//! than depending on whatever provides it. The real implementation lives in
//! `axon-sync` (it needs a live `Client` to read the sender's device + identity),
//! adapted onto this port by `axon-server`, so this crate stays free of
//! `axon-sync` and `matrix-sdk`.
//!
//! Two facts, deliberately separate (ADR 0031): the **snapshot** is what Matrix's
//! evidence said when the event arrived (immutable, persisted by Part A); the
//! **current** evidence is read live and can differ (a device trusted when it
//! sent can later be revoked, and vice versa).

use async_trait::async_trait;
use uuid::Uuid;

/// What can go wrong assembling a trust bundle. Small and HTTP-shaped, like
/// [`VerifyError`](crate::verification::VerifyError); the adapter collapses its
/// richer backend error into one of these (see the `From` impl in
/// [`response`](crate::response)).
#[derive(Debug)]
pub enum TrustError {
    /// No such account, or no such event for the account. → `404`.
    NotFound(String),
    /// The account is logged out (`deactivated`) or mid-teardown (`deleting`), so
    /// it has no live client to read trust evidence from. → `409`.
    NotActive(String),
    /// The upstream homeserver / SDK failed reading the sender's keys. → `502`.
    Upstream(String),
    /// An internal failure (e.g. the store). The detail is logged, not returned.
    /// → `500`.
    Internal,
}

/// The at-decrypt snapshot half of a bundle — present only when the event was
/// decrypted and carried a crypto sibling row (`None` for unencrypted events and
/// for a UTD not yet re-decrypted).
#[derive(Debug, Clone)]
pub struct TrustSnapshot {
    /// The sender-trust verdict at decrypt time (the same values as the
    /// timeline event's `sender_trust`).
    pub sender_trust: Option<String>,
    /// The coarse `verified`/`unverified` verification state at decrypt time.
    pub verification_state: Option<String>,
    /// The sending device's id at decrypt time.
    pub device_id: Option<String>,
    /// The sending device's curve25519 identity key.
    pub curve25519_key: Option<String>,
    /// The sending device's claimed ed25519 signing key.
    pub ed25519_key: Option<String>,
    /// The Megolm session id the event was encrypted with.
    pub session_id: Option<String>,
    /// Whether the Megolm key reached us forwarded (key-share) rather than
    /// directly from the sender's device.
    pub forwarded: Option<bool>,
    /// If forwarded, the user id that forwarded the key.
    pub forwarder_user_id: Option<String>,
    /// If forwarded, the device id that forwarded the key.
    pub forwarder_device_id: Option<String>,
}

/// The live evidence half of a bundle — read from the SDK at request time.
#[derive(Debug, Clone)]
pub struct CurrentTrust {
    /// Whether the sending device is currently known to the SDK (a deleted or
    /// not-yet-downloaded device is `false`). Always `false` for a
    /// `mismatched_sender` snapshot: the lookup is skipped for that verdict.
    pub device_known: bool,
    /// Whether the sending device is currently cross-signed by the sender's own
    /// master key. `None` when the device isn't known.
    pub device_cross_signed: Option<bool>,
    /// Whether the sender's user identity is currently known to the SDK.
    pub identity_known: bool,
    /// Whether the sender's identity is currently verified by us. `None` when the
    /// identity isn't known.
    pub identity_verified: Option<bool>,
    /// Whether the sender's identity is currently in a verification violation
    /// (was previously verified, identity since changed). `None` when unknown.
    pub verification_violation: Option<bool>,
    /// Whether the sender's identity was ever previously verified. `None` when
    /// unknown.
    pub previously_verified: Option<bool>,
    /// The sender's current master cross-signing key (base64), if known.
    pub master_key: Option<String>,
}

/// A per-event trust bundle: the durable at-decrypt snapshot plus live evidence.
#[derive(Debug, Clone)]
pub struct TrustBundle {
    /// Matrix user id of the event's sender.
    pub sender: String,
    /// The at-decrypt snapshot, if one was recorded.
    pub snapshot: Option<TrustSnapshot>,
    /// The current (live) trust evidence.
    pub current: CurrentTrust,
}

/// Assembles a per-event sender-trust bundle. Implemented outside this crate;
/// held in [`AppState`](crate::AppState) as `Arc<dyn SenderTrustService>`.
#[async_trait]
pub trait SenderTrustService: Send + Sync {
    /// Build the trust bundle for `(account_id, event_id)`. An unknown account or
    /// event is [`NotFound`](TrustError::NotFound); a logged-out / mid-teardown
    /// account is [`NotActive`](TrustError::NotActive).
    async fn bundle(&self, account_id: Uuid, event_id: &str) -> Result<TrustBundle, TrustError>;
}
