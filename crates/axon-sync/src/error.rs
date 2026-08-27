//! Sync-engine errors.

use thiserror::Error;
use uuid::Uuid;

/// Errors raised by the message gateway (the `SdkGateway` send path) and the
/// client manager that backs it. Wire-neutral: the API layer's composition-root
/// adapter (`axon-server`) maps these onto its own `MessageSender` error so
/// `axon-api` never depends on this crate. Variants are chosen to map cleanly to
/// HTTP status: unknown account / room → 404, not-yet-connected → 503, bad input
/// → 400, an upstream homeserver failure → 502.
#[derive(Debug, Error)]
pub enum GatewayError {
    /// No account row exists for this id.
    #[error("no such account: {0}")]
    UnknownAccount(Uuid),

    /// The account row exists but is not `active` (it is deactivated or being
    /// deleted), so it will not connect or send. Not retryable without a login
    /// to reactivate it (ADR 0022).
    #[error("account not active: {0}")]
    AccountNotActive(Uuid),

    /// The account exists but could not be brought online (homeserver
    /// unreachable, auth/restore failed, store error). Transient — retryable.
    #[error("account not connected: {0}")]
    NotConnected(String),

    /// The addressed room is unknown to this account's client (not joined yet).
    #[error("room not found: {0}")]
    RoomNotFound(String),

    /// The addressed media object no longer exists on the homeserver.
    #[error("media not found: {0}")]
    MediaNotFound(String),

    /// The operation isn't permitted: an attempt to edit a message the account
    /// didn't author, or a homeserver `M_FORBIDDEN` (e.g. redacting without the
    /// required power level).
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// A malformed parameter (e.g. an unparseable room or event id).
    #[error("invalid request: {0}")]
    Invalid(String),

    /// The homeserver rejected or failed the operation.
    #[error("upstream homeserver error: {0}")]
    Upstream(String),
}

impl From<SyncError> for GatewayError {
    /// A failure to build/authenticate a client surfaces as "not connected" —
    /// it's the transient, retryable class from the caller's perspective.
    fn from(err: SyncError) -> Self {
        GatewayError::NotConnected(err.to_string())
    }
}

impl From<GatewayError> for SyncError {
    /// The supervisor drives connects through the manager but reports failures as
    /// plain sync errors so its backoff/restart logic is unchanged.
    fn from(err: GatewayError) -> Self {
        SyncError::Sdk(err.to_string())
    }
}

/// Errors raised by the sync engine.
#[derive(Debug, Error)]
pub enum SyncError {
    /// An error surfaced by matrix-rust-sdk (login, restore, client build, or
    /// the running sync service). The SDK's own error type is large and not
    /// `'static`-friendly to carry around, so we keep its string form.
    #[error("matrix sdk error: {0}")]
    Sdk(String),

    /// The homeserver exposes neither Matrix OAuth authentication-metadata
    /// endpoint, so QR login is unavailable rather than transiently failing.
    #[error("homeserver does not advertise Matrix OAuth authentication")]
    MatrixOAuthUnavailable,

    /// Operator-controlled Matrix OAuth configuration is invalid. This stays
    /// distinct from an upstream failure so QR acquisition reports the fault as
    /// local without exposing the invalid value.
    #[error("invalid Matrix OAuth configuration")]
    MatrixOAuthConfiguration,

    /// Axon's persisted OAuth registration state is invalid or cannot be used.
    /// The presentation-safe message deliberately excludes the stored value.
    #[error("invalid local Matrix OAuth registration state")]
    MatrixOAuthLocalState,

    /// A login was rejected by the homeserver because the credentials were wrong
    /// (an `M_FORBIDDEN` / `M_UNAUTHORIZED` from the login call), as opposed to a
    /// transient connection failure. Lets the lifecycle layer return `401` rather
    /// than a generic upstream error.
    #[error("authentication failed: {0}")]
    AuthFailed(String),

    /// A storage-layer error while provisioning accounts or persisting tokens.
    #[error("store error: {0}")]
    Store(#[from] axon_store::StoreError),

    /// An account exists but `sync.store_key` is absent, so there is no key to
    /// encrypt the access token at rest or to passphrase the SDK store.
    #[error("sync.store_key is required once an account exists")]
    MissingStoreKey,

    /// A token-restore was attempted for an account with no stored `device_id`
    /// (the id the token belongs to).
    #[error("account {0}: a device_id is required to restore a session")]
    MissingDeviceId(String),

    /// No stored session exists for an account, so it cannot authenticate. Every
    /// account is minted with a session already persisted (by `login` or
    /// `import_token`), so this indicates a row created some other way.
    #[error("account {0}: no stored session")]
    NoCredential(String),
}

impl From<SyncError> for axon_core::Error {
    fn from(err: SyncError) -> Self {
        axon_core::Error::Sync(err.to_string())
    }
}

/// Shorthand for turning an SDK error into [`SyncError::Sdk`].
pub(crate) fn sdk_err(err: impl std::fmt::Display) -> SyncError {
    SyncError::Sdk(err.to_string())
}
