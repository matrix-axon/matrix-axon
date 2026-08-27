//! The account-lifecycle port the lifecycle handlers depend on.
//!
//! Like [`MessageSender`](crate::sender::MessageSender), `axon-api` defines the
//! capability it *needs* — adding/reactivating a Matrix account at runtime —
//! rather than depending on whatever provides it. The real implementation lives
//! in `axon-sync`, adapted onto this port by `axon-server`, so this crate stays
//! free of `axon-sync` and `matrix-sdk`.
//!
//! Operations return the affected Axon `account_id` on success; failures are
//! [`LoginError`], whose variants map 1:1 to HTTP status in
//! [`response`](crate::response).

use async_trait::async_trait;
use uuid::Uuid;

/// What can go wrong logging an account in. Deliberately small and HTTP-shaped:
/// the adapter that implements [`AccountLifecycle`] collapses its richer backend
/// error into one of these so the handler layer maps a stable set of statuses.
#[derive(Debug)]
pub enum LoginError {
    /// The request was malformed — e.g. `username` is not a valid Matrix user ID.
    /// → `400`.
    InvalidRequest(String),
    /// The homeserver rejected the supplied credentials. → `401`.
    AuthFailed(String),
    /// An account for this identity is already active (or being deleted), so it
    /// must be logged out before logging in again. → `409`.
    Conflict(String),
    /// The upstream homeserver was unreachable or failed the login. → `502`.
    Upstream(String),
    /// An internal failure (e.g. the store). The detail is logged, not returned.
    /// → `500`.
    Internal,
}

/// What can go wrong logging an account out. Like [`LoginError`], small and
/// HTTP-shaped; the adapter collapses its backend error into one of these.
#[derive(Debug)]
pub enum LogoutError {
    /// No account exists for the given id. → `404`.
    NotFound(String),
    /// The account is mid-teardown (a delete is in flight), so it can't be logged
    /// out. → `409`.
    Conflict(String),
    /// An internal failure (e.g. the store). The detail is logged, not returned.
    /// → `500`.
    ///
    /// There is deliberately no upstream/502 variant: the backend invalidates the
    /// device token upstream best-effort and never fails logout over it, so such
    /// a variant would be unreachable API surface.
    Internal,
}

/// What can go wrong deleting an account. Like [`LogoutError`], small and
/// HTTP-shaped; the adapter collapses its backend error into one of these.
#[derive(Debug)]
pub enum DeleteError {
    /// No account exists for the given id. → `404`. (Also what a second concurrent
    /// delete sees once the first has removed the row.)
    NotFound(String),
    /// The teardown could not finish with its postcondition intact — the account's
    /// supervised task has not yet terminated, so its store dir can't be removed.
    /// Transient: a retry (or the next boot's reconcile) completes it. → `409`.
    Conflict(String),
    /// An internal failure (e.g. the store, or removing the on-disk store dir). The
    /// detail is logged, not returned. → `500`.
    ///
    /// As with [`LogoutError`] there is deliberately no upstream/502 variant: the
    /// device-token invalidation is best-effort and never fails the delete.
    Internal,
}

/// What can go wrong acquiring keys from a recovery key. Like the others, small
/// and HTTP-shaped; the adapter collapses its backend error into one of these.
#[derive(Debug)]
pub enum RecoverError {
    /// No account exists for the given id. → `404`.
    NotFound(String),
    /// The account can't be recovered right now: it is logged out
    /// (`deactivated` — log in first) or mid-teardown (`deleting`). → `409`.
    Conflict(String),
    /// The recovery key was wrong/rotated, the account never set up Secure
    /// Backup, `recovery_key` was required and missing, or enabling would mint
    /// a new recovery key. A client error, not an internal one — and never a
    /// silent permanent UTD. → `400`.
    BadRequest(String),
    /// An internal failure (e.g. the store, or the live client could not be
    /// reached). The detail is logged, not returned. → `500`.
    Internal,
}

/// What can go wrong retrying pending UTD re-decryption. Like recovery, this
/// needs a live active account but takes no secret-bearing input.
#[derive(Debug)]
pub enum RedecryptUtdsError {
    /// No account exists for the given id. → `404`.
    NotFound(String),
    /// The account is logged out or mid-teardown. → `409`.
    Conflict(String),
    /// An internal failure (e.g. the store, or the live client could not be
    /// reached). The detail is logged, not returned. → `500`.
    Internal,
}

/// Counts returned by a manual UTD re-decryption retry.
#[derive(Debug, Clone, Copy, Default)]
pub struct RedecryptUtdsStats {
    pub selected: usize,
    pub attempted: usize,
    pub decrypted: usize,
    pub still_pending: usize,
    pub timed_out: bool,
}

/// What a recover or enable-backup request did about megolm backup (ADR 0098).
/// Mapped onto [`crate::dto::BackupActionDto`] at the handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupAction {
    Joined,
    Enabled,
    ExportPending,
    Failed,
    AlreadyUploading,
}

/// Result of a successful recover: 4S import happened; backup action and the
/// under-lock UTD sweep summary are honesty, not a second success criterion.
#[derive(Debug, Clone, Copy)]
pub struct RecoverResult {
    pub redecrypt: RedecryptUtdsStats,
    pub backup_action: BackupAction,
}

/// Adds, reactivates, stops, or removes a Matrix account at runtime. Implemented
/// outside this crate; held in [`AppState`](crate::AppState) as
/// `Arc<dyn AccountLifecycle>`.
#[async_trait]
pub trait AccountLifecycle: Send + Sync {
    /// Log in (or reactivate) the account identified by `username`, a full Matrix
    /// user ID. A retained account continues using its stored homeserver endpoint.
    /// When no account exists and
    /// `homeserver_url` is `None`, the implementation discovers the canonical
    /// homeserver from the user ID's server name (`.well-known/matrix/client`,
    /// falling back to the server name itself); a username whose domain is the
    /// homeserver's hostname is an [`InvalidRequest`](LoginError::InvalidRequest)
    /// whose message suggests the canonical spelling. Returns the Axon
    /// `account_id` of the now-active account.
    async fn login(
        &self,
        homeserver_url: Option<&str>,
        username: &str,
        password: &str,
    ) -> Result<Uuid, LoginError>;

    /// Adopt an existing Matrix `access_token` + `device_id` as a runtime
    /// account — the runtime replacement for the retired
    /// `sync.account.access_token` boot-provisioning path: restore a session
    /// axon didn't mint itself (e.g. one issued by another client, or an
    /// SSO-only account with no password) without a fresh login. Same
    /// idempotency as [`login`](Self::login) (new identity → mint; `active` →
    /// no-op; `deactivated` → reactivate; `deleting` → conflict), but
    /// `homeserver_url` is required (there is no MXID-based discovery for a
    /// token) and only consulted for a new identity. Unlike `login`, no
    /// homeserver call confirms the token up front — the implementation
    /// validates it (and, if reported, the device id) before returning
    /// success, surfacing a mismatched or revoked token as
    /// [`AuthFailed`](LoginError::AuthFailed).
    async fn import_token(
        &self,
        homeserver_url: &str,
        username: &str,
        access_token: &str,
        device_id: &str,
    ) -> Result<Uuid, LoginError>;

    /// Log the account `account_id` out: stop syncing it, invalidate its device
    /// token upstream, and move it to a logged-out (`deactivated`) state, retaining
    /// all of its data so a later [`login`](Self::login) reactivates it. Idempotent
    /// — logging out an already-logged-out account succeeds.
    async fn logout(&self, account_id: Uuid) -> Result<(), LogoutError>;

    /// Acquire E2EE keys for the **active** account `account_id` from its
    /// Secure-Storage (4S) `recovery_key`: import cross-signing (self-verifying
    /// axon's device), auto-enable megolm backup when the homeserver has none
    /// (ADR 0098), and back-fill any stored UTDs the keys now unlock. The
    /// `recovery_key` is consumed once and never persisted. A logged-out
    /// (`deactivated`) or mid-teardown (`deleting`) account is a
    /// [`Conflict`](RecoverError::Conflict); a wrong/rotated key (or no
    /// Secure Backup) is a [`BadRequest`](RecoverError::BadRequest). A 200
    /// means 4S import succeeded — `backup_action` and `redecrypt` tell the
    /// rest of the truth.
    async fn recover(
        &self,
        account_id: Uuid,
        recovery_key: &str,
    ) -> Result<RecoverResult, RecoverError>;

    /// Originate, join-retry (export-only / crash-resume), or kick upload for
    /// megolm key backup on an **active, verified** account (ADR 0098).
    /// `recovery_key` is required for create, export-only, and replace; omit
    /// it to kick an already-enabled upload. Unverified, not-ours HS backup,
    /// or not-active → [`Conflict`](RecoverError::Conflict). Missing key when
    /// 4S export is needed, or no 4S at all → [`BadRequest`](RecoverError::BadRequest).
    async fn enable_backup(
        &self,
        account_id: Uuid,
        recovery_key: Option<&str>,
    ) -> Result<BackupAction, RecoverError>;

    /// Explicitly retry every pending UTD for an active account. This is the
    /// operator path for rows that the default startup sweep already attempted
    /// once, or for keys that are already in the crypto store without a fresh
    /// room-key arrival event.
    async fn redecrypt_utds(
        &self,
        account_id: Uuid,
    ) -> Result<RedecryptUtdsStats, RedecryptUtdsError>;

    /// Permanently delete the account `account_id` and every trace of it — an
    /// ordered, crash-recoverable teardown (the Postgres archive via cascade, the
    /// on-disk SDK store). Unlike [`logout`](Self::logout) this is *not* reversible:
    /// re-adding the same Matrix account later is a fresh [`login`](Self::login)
    /// with a new id. Idempotent — a delete of an in-flight (`deleting`) row resumes
    /// it; a delete of an already-gone row is a [`NotFound`](DeleteError::NotFound).
    async fn delete(&self, account_id: Uuid) -> Result<(), DeleteError>;
}
