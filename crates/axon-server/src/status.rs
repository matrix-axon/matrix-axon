//! Composition-root adapters: bind `axon-sync`'s [`BackfillHealth`] and
//! [`SyncHealth`] to `axon-api`'s status ports (M10), so `GET /v1/status`
//! reports the engine's disk-space health and each account's sync-service
//! state. Also binds [`BackupHealth`] to `AccountDto.backup` (ADR 0098).
//! This binary is the one place that knows both crates; `axon-api` and
//! `axon-sync` never depend on each other.

use async_trait::async_trait;
use axon_api::{
    AccountSyncSnapshot, BackfillStatusProvider, BackfillStatusSnapshot, BackupSnapshotDto,
    BackupStateDto, BackupStateProvider, RecoveryStateDto, SyncStateProvider, SyncStatusProvider,
};
use axon_sync::{
    BackfillHealth, BackupHealth, BackupSnapshot, BackupStateView, RecoveryStateView, SyncHealth,
};
use uuid::Uuid;

/// Wraps the sync engine's shared [`BackfillHealth`] so it satisfies the API's
/// [`BackfillStatusProvider`] port. The orphan rule requires a local newtype.
pub struct BackfillStatusAdapter(pub BackfillHealth);

impl BackfillStatusProvider for BackfillStatusAdapter {
    fn snapshot(&self) -> BackfillStatusSnapshot {
        BackfillStatusSnapshot {
            paused_low_disk: self.0.paused_low_disk(),
            free_bytes: self.0.free_bytes(),
        }
    }
}

/// Wraps the sync engine's shared [`SyncHealth`] so it satisfies the API's
/// [`SyncStatusProvider`] port. The orphan rule requires a local newtype.
pub struct SyncStatusAdapter(pub SyncHealth);

impl SyncStatusProvider for SyncStatusAdapter {
    fn snapshot(&self) -> Vec<AccountSyncSnapshot> {
        self.0
            .snapshot()
            .into_iter()
            .map(|(account_id, status)| AccountSyncSnapshot {
                account_id,
                state: status.state.as_str(),
                since: status.since,
            })
            .collect()
    }
}

/// Wraps the sync engine's shared [`SyncHealth`] so it satisfies the API's
/// [`SyncStateProvider`] port (ADR 0030, issue #241) — the same handle
/// [`SyncStatusAdapter`] wraps above, since `SyncHealth` derives both the raw
/// `/v1/status` surface and the coarser `AccountDto.sync_state` vocabulary.
/// The orphan rule requires a local newtype.
pub struct SyncStateAdapter(pub SyncHealth);

impl SyncStateProvider for SyncStateAdapter {
    fn sync_state(&self, account_id: Uuid) -> &'static str {
        self.0.sync_state(account_id)
    }
}

/// Wraps the sync engine's [`BackupHealth`] so it satisfies the API's
/// [`BackupStateProvider`] port (ADR 0098).
pub struct BackupStateAdapter(pub BackupHealth);

#[async_trait]
impl BackupStateProvider for BackupStateAdapter {
    async fn snapshot(&self, account_id: Uuid) -> BackupSnapshotDto {
        map_backup_snapshot(self.0.snapshot(account_id).await)
    }
}

fn map_backup_snapshot(snapshot: BackupSnapshot) -> BackupSnapshotDto {
    BackupSnapshotDto {
        exists_on_server: snapshot.exists_on_server,
        this_device_uploading: snapshot.this_device_uploading,
        backup_state: map_backup_state(snapshot.backup_state),
        recovery_state: map_recovery_state(snapshot.recovery_state),
    }
}

fn map_backup_state(state: BackupStateView) -> BackupStateDto {
    match state {
        BackupStateView::Unknown => BackupStateDto::Unknown,
        BackupStateView::Creating => BackupStateDto::Creating,
        BackupStateView::Enabling => BackupStateDto::Enabling,
        BackupStateView::Resuming => BackupStateDto::Resuming,
        BackupStateView::Enabled => BackupStateDto::Enabled,
        BackupStateView::Downloading => BackupStateDto::Downloading,
        BackupStateView::Disabling => BackupStateDto::Disabling,
    }
}

fn map_recovery_state(state: RecoveryStateView) -> RecoveryStateDto {
    match state {
        RecoveryStateView::Unknown => RecoveryStateDto::Unknown,
        RecoveryStateView::Enabled => RecoveryStateDto::Enabled,
        RecoveryStateView::Disabled => RecoveryStateDto::Disabled,
        RecoveryStateView::Incomplete => RecoveryStateDto::Incomplete,
    }
}
