//! Composition-root adapter: binds `axon-sync`'s concrete [`AccountLifecycle`]
//! (the runtime login engine) to `axon-api`'s lifecycle port.
//!
//! Same shape as the [`GatewayAdapter`](crate::gateway::GatewayAdapter): this
//! binary is the one place that knows both crates, so the adapter and the error
//! translation live here. `axon-api` and `axon-sync` never depend on each other.

use async_trait::async_trait;
use axon_api::{
    AccountLifecycle, BackupAction, DeleteError, LoginError, LogoutError, RecoverError,
    RecoverResult, RedecryptUtdsError, RedecryptUtdsStats,
};
use axon_sync::{
    AccountLifecycle as SyncLifecycle, BackupAction as SyncBackupAction, LifecycleError,
    RedecryptSummary,
};
use uuid::Uuid;

/// Wraps the sync engine's lifecycle so it satisfies the API's `AccountLifecycle`
/// port. The orphan rule requires a local newtype to carry the impl.
pub struct LifecycleAdapter(pub SyncLifecycle);

/// Map a sync-layer lifecycle error onto the API-layer login error (and thus an
/// HTTP status): a bad MXID → invalid request, an account mid-teardown → conflict,
/// rejected credentials → auth failure, a homeserver failure → upstream, a store
/// failure → a logged internal error.
fn map_login_err(err: LifecycleError) -> LoginError {
    match err {
        LifecycleError::InvalidUserId(msg) => LoginError::InvalidRequest(msg),
        LifecycleError::BeingDeleted(id) => {
            LoginError::Conflict(format!("account is being deleted: {id}"))
        }
        LifecycleError::AlreadyActive(id) => {
            LoginError::Conflict(format!("account is already active: {id}"))
        }
        LifecycleError::LoginFinalizing(user_id) => LoginError::Conflict(format!(
            "a Matrix OAuth QR login is still finalizing for {user_id}"
        )),
        LifecycleError::AuthFailed(msg) => LoginError::AuthFailed(msg),
        LifecycleError::Upstream(msg) => LoginError::Upstream(msg),
        // A previous task for this identity hasn't terminated yet; the store dir
        // it holds can't be restaged. Transient — a logout retry reaps it.
        LifecycleError::Draining(id) => LoginError::Conflict(format!(
            "a previous session for account {id} is still shutting down; retry shortly"
        )),
        LifecycleError::Store(msg) => {
            tracing::error!(error = %msg, "store error during account login");
            LoginError::Internal
        }
        // Login never surfaces these — `NotFound` (it mints a row for a new
        // identity) or `NotActive` / `RecoveryFailed` (recover-only) — so treat
        // them defensively as an internal error.
        LifecycleError::NotFound(id) | LifecycleError::NotActive(id) => {
            tracing::error!(%id, "unexpected lifecycle error from account login");
            LoginError::Internal
        }
        LifecycleError::RecoveryFailed(msg) | LifecycleError::BackupConflict(msg) => {
            tracing::error!(error = %msg, "unexpected recovery error from account login");
            LoginError::Internal
        }
        LifecycleError::DeviceNotVerified => {
            tracing::error!("unexpected QR-only trust error from account login");
            LoginError::Internal
        }
    }
}

/// Map a sync-layer lifecycle error onto the API-layer logout error: an unknown id
/// → not found, an account mid-teardown → conflict, a store failure → a logged
/// internal error.
fn map_logout_err(err: LifecycleError) -> LogoutError {
    match err {
        LifecycleError::NotFound(id) => LogoutError::NotFound(format!("account {id} not found")),
        LifecycleError::BeingDeleted(id) => {
            LogoutError::Conflict(format!("account is being deleted: {id}"))
        }
        // The task survived cancel + abort, so the logout could not complete
        // with its postcondition intact. Transient — retrying reaps it again.
        LifecycleError::Draining(id) => LogoutError::Conflict(format!(
            "the session for account {id} is still shutting down; retry shortly"
        )),
        LifecycleError::Store(msg) => {
            tracing::error!(error = %msg, "store error during account logout");
            LogoutError::Internal
        }
        // Logout takes only an account id and never fails over the upstream
        // homeserver (token invalidation is best-effort), so a bad MXID, rejected
        // credential, or upstream error can't arise; treat them defensively as an
        // internal error.
        other => {
            tracing::error!(error = %other, "unexpected error during account logout");
            LogoutError::Internal
        }
    }
}

/// Map a sync-layer lifecycle error onto the API-layer delete error: an unknown id
/// → not found, a not-yet-terminated sync task → conflict, a store/dir-removal
/// failure → a logged internal error.
fn map_delete_err(err: LifecycleError) -> DeleteError {
    match err {
        LifecycleError::NotFound(id) => DeleteError::NotFound(format!("account {id} not found")),
        // The account's task survived cancel + abort, so its store dir can't be
        // treated as quiescent and the teardown stopped before removing it. The row
        // stays `deleting`; a retry (or the next boot's reconcile) finishes it.
        LifecycleError::Draining(id) => DeleteError::Conflict(format!(
            "the sync task for account {id} is still shutting down; retry shortly"
        )),
        LifecycleError::Store(msg) => {
            tracing::error!(error = %msg, "store error during account delete");
            DeleteError::Internal
        }
        // Delete takes only an account id and resolves a `deleting` row by resuming
        // (never `BeingDeleted`); a bad MXID, rejected credential, or upstream error
        // can't arise. Treat them defensively as an internal error.
        other => {
            tracing::error!(error = %other, "unexpected error during account delete");
            DeleteError::Internal
        }
    }
}

/// Map a sync-layer lifecycle error onto the API-layer recover error: an unknown
/// id → not found, a not-active / mid-teardown account → conflict, a bad/rotated
/// recovery key → bad request, a store failure → a logged internal error.
fn map_recover_err(err: LifecycleError) -> RecoverError {
    match err {
        LifecycleError::NotFound(id) => RecoverError::NotFound(format!("account {id} not found")),
        // Logged out: there is no live session to recover against. Reversible —
        // the client should log in first, then recover.
        LifecycleError::NotActive(id) => RecoverError::Conflict(format!(
            "account {id} is not active; log in before recovering"
        )),
        LifecycleError::BeingDeleted(id) => {
            RecoverError::Conflict(format!("account is being deleted: {id}"))
        }
        // A previous task for this identity hasn't terminated; its store dir/client
        // can't be relied on. Transient — a logout retry reaps it.
        LifecycleError::Draining(id) => RecoverError::Conflict(format!(
            "the session for account {id} is still shutting down; retry shortly"
        )),
        // The only client-actionable recovery failure: a wrong/rotated key, or no
        // Secure Backup on the account (a readable 400, never a silent permanent
        // UTD). The sync layer already replaces the SDK's text with a stable message
        // here, so nothing secret-storage-internal leaks.
        LifecycleError::RecoveryFailed(msg) => RecoverError::BadRequest(msg),
        LifecycleError::BackupConflict(msg) => RecoverError::Conflict(msg),
        // The live client couldn't be reached, or the SDK failed the import for a
        // reason the caller can't fix by changing the key (a non-secret-storage
        // `RecoveryError`). Not the caller's fault → a generic 500, detail logged
        // server-side rather than returned. (A 502 would arguably fit a transient
        // homeserver failure, but the SDK's opaque `Sdk` variant doesn't cleanly
        // separate transient-upstream from internal, so we stay conservative.)
        LifecycleError::Upstream(msg) => {
            tracing::error!(error = %msg, "upstream/internal error during account recover");
            RecoverError::Internal
        }
        LifecycleError::Store(msg) => {
            tracing::error!(error = %msg, "store error during account recover");
            RecoverError::Internal
        }
        // Recover takes only an account id + recovery key; a bad MXID, rejected
        // credential, or the delete-only provisioned guard can't arise. Treat them
        // defensively as an internal error.
        other => {
            tracing::error!(error = %other, "unexpected error during account recover");
            RecoverError::Internal
        }
    }
}

/// Map a sync-layer lifecycle error onto the API-layer manual UTD retry error:
/// an unknown id → not found, a logged-out / mid-teardown account → conflict, and
/// store/client failures → a logged internal error.
fn map_redecrypt_err(err: LifecycleError) -> RedecryptUtdsError {
    match err {
        LifecycleError::NotFound(id) => {
            RedecryptUtdsError::NotFound(format!("account {id} not found"))
        }
        LifecycleError::NotActive(id) => RedecryptUtdsError::Conflict(format!(
            "account {id} is not active; log in before retrying UTD re-decryption"
        )),
        LifecycleError::BeingDeleted(id) => {
            RedecryptUtdsError::Conflict(format!("account is being deleted: {id}"))
        }
        LifecycleError::Draining(id) => RedecryptUtdsError::Conflict(format!(
            "the session for account {id} is still shutting down; retry shortly"
        )),
        LifecycleError::Store(msg) => {
            tracing::error!(error = %msg, "store error during manual UTD re-decryption");
            RedecryptUtdsError::Internal
        }
        LifecycleError::Upstream(msg) => {
            tracing::error!(error = %msg, "client error during manual UTD re-decryption");
            RedecryptUtdsError::Internal
        }
        other => {
            tracing::error!(error = %other, "unexpected error during manual UTD re-decryption");
            RedecryptUtdsError::Internal
        }
    }
}

fn map_redecrypt_summary(summary: RedecryptSummary) -> RedecryptUtdsStats {
    RedecryptUtdsStats {
        selected: summary.selected,
        attempted: summary.attempted,
        decrypted: summary.decrypted,
        still_pending: summary.still_pending,
        timed_out: summary.timed_out,
    }
}

fn map_backup_action(action: SyncBackupAction) -> BackupAction {
    match action {
        SyncBackupAction::Joined => BackupAction::Joined,
        SyncBackupAction::Enabled => BackupAction::Enabled,
        SyncBackupAction::ExportPending => BackupAction::ExportPending,
        SyncBackupAction::Failed => BackupAction::Failed,
        SyncBackupAction::AlreadyUploading => BackupAction::AlreadyUploading,
    }
}

#[async_trait]
impl AccountLifecycle for LifecycleAdapter {
    async fn login(
        &self,
        homeserver_url: Option<&str>,
        username: &str,
        password: &str,
    ) -> Result<Uuid, LoginError> {
        self.0
            .login(homeserver_url, username, password)
            .await
            .map_err(map_login_err)
    }

    async fn import_token(
        &self,
        homeserver_url: &str,
        username: &str,
        access_token: &str,
        device_id: &str,
    ) -> Result<Uuid, LoginError> {
        self.0
            .import_token(homeserver_url, username, access_token, device_id)
            .await
            .map_err(map_login_err)
    }

    async fn logout(&self, account_id: Uuid) -> Result<(), LogoutError> {
        self.0.logout(account_id).await.map_err(map_logout_err)
    }

    async fn delete(&self, account_id: Uuid) -> Result<(), DeleteError> {
        self.0.delete(account_id).await.map_err(map_delete_err)
    }

    async fn recover(
        &self,
        account_id: Uuid,
        recovery_key: &str,
    ) -> Result<RecoverResult, RecoverError> {
        self.0
            .recover(account_id, recovery_key)
            .await
            .map(|(backup_action, summary)| RecoverResult {
                redecrypt: map_redecrypt_summary(summary),
                backup_action: map_backup_action(backup_action),
            })
            .map_err(map_recover_err)
    }

    async fn enable_backup(
        &self,
        account_id: Uuid,
        recovery_key: Option<&str>,
    ) -> Result<BackupAction, RecoverError> {
        self.0
            .enable_backup(account_id, recovery_key)
            .await
            .map(map_backup_action)
            .map_err(map_recover_err)
    }

    async fn redecrypt_utds(
        &self,
        account_id: Uuid,
    ) -> Result<RedecryptUtdsStats, RedecryptUtdsError> {
        self.0
            .redecrypt_utds(account_id)
            .await
            .map(map_redecrypt_summary)
            .map_err(map_redecrypt_err)
    }
}
