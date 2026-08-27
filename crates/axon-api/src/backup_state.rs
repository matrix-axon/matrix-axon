//! Live megolm-backup observation port (ADR 0098), for `AccountDto.backup`.
//!
//! Distinct from [`crate::sync_state`] (a synchronous in-memory read with no
//! I/O): backup probes are async and can hit the homeserver, so this port is
//! bounded, deactivated-short-circuit, and stubbed in API tests. Like
//! `sync_state`, the API depends only on this port; the binary injects an
//! adapter over `axon-sync`.

use async_trait::async_trait;
use uuid::Uuid;

use crate::dto::BackupSnapshotDto;

/// Port exposing each account's live megolm-backup snapshot (ADR 0098) to the
/// accounts routes. Orthogonal to `verified`.
#[async_trait]
pub trait BackupStateProvider: Send + Sync {
    /// Live megolm-backup observation for `account_id`.
    ///
    /// A deactivated / deleting / unknown account must return
    /// [`BackupSnapshotDto::unknown`] and must not call the homeserver.
    /// A hung or failed probe yields `exists_on_server: null` for that row
    /// and must not pin a list of accounts.
    async fn snapshot(&self, account_id: Uuid) -> BackupSnapshotDto;
}

/// Fallback provider used when the binary injects none (e.g. in API tests):
/// every account reports the deactivated-shaped unknown snapshot.
pub(crate) struct NoBackupState;

#[async_trait]
impl BackupStateProvider for NoBackupState {
    async fn snapshot(&self, _account_id: Uuid) -> BackupSnapshotDto {
        BackupSnapshotDto::unknown()
    }
}
