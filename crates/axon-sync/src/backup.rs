//! Megolm key-backup origin, join, export-resume, and live snapshot (ADR 0098).
//!
//! `recovery().enable_backup()` is not 4S export: it creates a homeserver
//! backup version and a local decryption key. `SecretStore::export_secrets()`
//! writes `m.megolm_backup.v1` into already-open 4S. Decision trees are pure
//! so they can be unit-tested without a homeserver; SDK I/O lives in
//! [`AccountLifecycle`](crate::lifecycle::AccountLifecycle).

use std::time::Duration;

use axon_store::{Account, AccountState, Store};
use matrix_sdk::encryption::backups::BackupState;
use matrix_sdk::encryption::recovery::RecoveryState;
use matrix_sdk::encryption::secret_storage::SecretStore;
use matrix_sdk::ruma::api::client::backup::{
    delete_backup_version, get_latest_backup_info, BackupAlgorithm,
};
use matrix_sdk::ruma::events::secret::request::SecretName;
use matrix_sdk::ruma::UserId;
use matrix_sdk::Client;
use uuid::Uuid;

use crate::lifecycle::LifecycleError;
use crate::manager::ClientManager;

/// Bound on GET/list backup probes (are_enabled + cached exists_on_server).
pub(crate) const SNAPSHOT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound on enable_backup + export_secrets under the identity lock.
pub(crate) const ENABLE_EXPORT_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on wait_for_steady_state after the enable verb drops the lock.
pub(crate) const UPLOAD_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// What a recover / enable-backup request did about megolm backup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupAction {
    Joined,
    Enabled,
    ExportPending,
    Failed,
    AlreadyUploading,
}

/// Live megolm-backup observation. Orthogonal to `verified`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupSnapshot {
    pub exists_on_server: Option<bool>,
    pub this_device_uploading: bool,
    pub backup_state: BackupStateView,
    pub recovery_state: RecoveryStateView,
}

impl BackupSnapshot {
    pub fn unknown() -> Self {
        Self {
            exists_on_server: None,
            this_device_uploading: false,
            backup_state: BackupStateView::Unknown,
            recovery_state: RecoveryStateView::Unknown,
        }
    }
}

/// Closed mapping of SDK [`BackupState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupStateView {
    Unknown,
    Creating,
    Enabling,
    Resuming,
    Enabled,
    Downloading,
    Disabling,
}

impl From<BackupState> for BackupStateView {
    fn from(state: BackupState) -> Self {
        match state {
            BackupState::Unknown => Self::Unknown,
            BackupState::Creating => Self::Creating,
            BackupState::Enabling => Self::Enabling,
            BackupState::Resuming => Self::Resuming,
            BackupState::Enabled => Self::Enabled,
            BackupState::Downloading => Self::Downloading,
            BackupState::Disabling => Self::Disabling,
        }
    }
}

/// Closed mapping of SDK [`RecoveryState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStateView {
    Unknown,
    Enabled,
    Disabled,
    Incomplete,
}

impl From<RecoveryState> for RecoveryStateView {
    fn from(state: RecoveryState) -> Self {
        match state {
            RecoveryState::Unknown => Self::Unknown,
            RecoveryState::Enabled => Self::Enabled,
            RecoveryState::Disabled => Self::Disabled,
            RecoveryState::Incomplete => Self::Incomplete,
        }
    }
}

/// Which verb's decision tree to run. Recover never 409s a join.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackupVerb {
    Recover,
    Enable,
}

/// Observations the recover/enable trees consume. Pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BackupProbe {
    pub verified: bool,
    pub recovery_key_present: bool,
    pub are_enabled: bool,
    pub exists_on_server: bool,
    /// `None` when 4S was not opened (omit-key enable).
    pub four_s_has_megolm: Option<bool>,
    pub intent: bool,
    /// `None` when we did not inspect `auth_data`.
    pub signed_by_this_device: Option<bool>,
}

/// Next step after a probe. Pure; SDK I/O is the caller's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackupPlan {
    EnableAndExport,
    ExportOnly,
    AlreadyUploading,
    Joined,
    ReplaceThenEnable,
    Failed,
    RefuseJoin,
    NeedRecoveryKey,
    Unverified,
}

/// Recover never 409s a join; enable may.
pub(crate) fn plan_backup(verb: BackupVerb, probe: &BackupProbe) -> BackupPlan {
    match verb {
        BackupVerb::Enable => plan_enable(probe),
        BackupVerb::Recover => plan_recover(probe),
    }
}

fn plan_enable(probe: &BackupProbe) -> BackupPlan {
    if !probe.verified {
        return BackupPlan::Unverified;
    }
    if !probe.recovery_key_present {
        return if probe.are_enabled {
            BackupPlan::AlreadyUploading
        } else {
            BackupPlan::NeedRecoveryKey
        };
    }
    if probe.are_enabled {
        return if probe.four_s_has_megolm == Some(false) {
            BackupPlan::ExportOnly
        } else {
            BackupPlan::AlreadyUploading
        };
    }
    if !probe.exists_on_server {
        return BackupPlan::EnableAndExport;
    }
    if probe.intent && probe.signed_by_this_device == Some(true) {
        BackupPlan::ReplaceThenEnable
    } else {
        BackupPlan::RefuseJoin
    }
}

fn plan_recover(probe: &BackupProbe) -> BackupPlan {
    if !probe.exists_on_server {
        return BackupPlan::EnableAndExport;
    }
    if probe.are_enabled && probe.four_s_has_megolm == Some(false) {
        return BackupPlan::ExportOnly;
    }
    if probe.are_enabled && probe.four_s_has_megolm != Some(false) {
        return BackupPlan::AlreadyUploading;
    }
    if probe.four_s_has_megolm == Some(true) {
        return BackupPlan::Joined;
    }
    if probe.intent && probe.signed_by_this_device == Some(true) {
        BackupPlan::ReplaceThenEnable
    } else {
        BackupPlan::Failed
    }
}

/// Cheap cloneable handle for GET/list snapshots (ADR 0098).
#[derive(Clone)]
pub struct BackupHealth {
    store: Store,
    manager: ClientManager,
}

impl BackupHealth {
    pub(crate) fn new(store: Store, manager: ClientManager) -> Self {
        Self { store, manager }
    }

    pub async fn snapshot(&self, account_id: Uuid) -> BackupSnapshot {
        let account = match self.store.get_account(account_id).await {
            Ok(Some(account)) => account,
            Ok(None) | Err(_) => return BackupSnapshot::unknown(),
        };
        if account.state != AccountState::Active {
            return BackupSnapshot::unknown();
        }
        let Some(client) = self.manager.cached(account_id) else {
            return BackupSnapshot::unknown();
        };
        snapshot_from_client(&client).await
    }
}

async fn snapshot_from_client(client: &Client) -> BackupSnapshot {
    let backups = client.encryption().backups();
    let recovery = client.encryption().recovery();
    let backup_state = BackupStateView::from(backups.state());
    let recovery_state = RecoveryStateView::from(recovery.state());
    match tokio::time::timeout(SNAPSHOT_PROBE_TIMEOUT, async {
        let uploading = backups.are_enabled().await;
        let exists = backups.exists_on_server().await.ok();
        (uploading, exists)
    })
    .await
    {
        Ok((uploading, exists)) => BackupSnapshot {
            exists_on_server: exists,
            this_device_uploading: uploading,
            backup_state,
            recovery_state,
        },
        Err(_) => BackupSnapshot {
            exists_on_server: None,
            this_device_uploading: false,
            backup_state,
            recovery_state,
        },
    }
}

pub(crate) async fn four_s_has_megolm(store: &SecretStore) -> bool {
    matches!(store.get_secret(SecretName::RecoveryKey).await, Ok(Some(_)))
}

/// Whether the current HS backup version's `auth_data` is signed by this
/// Axon device. Used only for crash-resume replace; never as a "keep because
/// count is zero" heuristic.
pub(crate) async fn current_backup_signed_by_this_device(
    client: &Client,
    account: &Account,
) -> Result<(String, bool, Option<u64>), LifecycleError> {
    let response = client
        .send(get_latest_backup_info::v3::Request::new())
        .await
        .map_err(|err| LifecycleError::Upstream(err.to_string()))?;
    let version = response.version.clone();
    let count = Some(u64::from(response.count));
    let signed = algorithm_signed_by_device(
        &response.algorithm,
        &account.user_id,
        account.device_id.as_deref(),
    );
    Ok((version, signed, count))
}

fn algorithm_signed_by_device(
    algorithm: &matrix_sdk::ruma::serde::Raw<BackupAlgorithm>,
    user_id: &str,
    device_id: Option<&str>,
) -> bool {
    let Some(device_id) = device_id else {
        return false;
    };
    let Ok(algo) = algorithm.deserialize() else {
        return false;
    };
    let BackupAlgorithm::MegolmBackupV1Curve25519AesSha2(auth) = algo else {
        return false;
    };
    let Ok(uid) = UserId::parse(user_id) else {
        return false;
    };
    auth.signatures.get(&uid).is_some_and(|sigs| {
        sigs.keys()
            .any(|key_id| key_id.key_name().as_str() == device_id)
    })
}

pub(crate) async fn delete_backup_version(
    client: &Client,
    version: String,
) -> Result<(), LifecycleError> {
    client
        .send(delete_backup_version::v3::Request::new(version))
        .await
        .map_err(|err| LifecycleError::Upstream(err.to_string()))?;
    Ok(())
}

/// After a successful `enable_backup()`, log if the current HS version is
/// unexpected. Do not `disable_and_delete`.
pub(crate) async fn log_post_create_version(client: &Client, account_id: Uuid) {
    match client
        .send(get_latest_backup_info::v3::Request::new())
        .await
    {
        Ok(info) => {
            tracing::info!(
                %account_id,
                version = %info.version,
                count = u64::from(info.count),
                "megolm backup version after enable_backup"
            );
        }
        Err(err) => {
            tracing::warn!(
                %account_id,
                error = %err,
                "failed to re-fetch megolm backup version after enable_backup"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(overrides: impl Fn(&mut BackupProbe)) -> BackupProbe {
        let mut p = BackupProbe {
            verified: true,
            recovery_key_present: true,
            are_enabled: false,
            exists_on_server: false,
            four_s_has_megolm: Some(false),
            intent: false,
            signed_by_this_device: None,
        };
        overrides(&mut p);
        p
    }

    #[test]
    fn recover_no_hs_backup_enables() {
        assert_eq!(
            plan_backup(BackupVerb::Recover, &probe(|_| {})),
            BackupPlan::EnableAndExport
        );
    }

    #[test]
    fn recover_joined_when_four_s_has_megolm() {
        let p = probe(|p| {
            p.exists_on_server = true;
            p.four_s_has_megolm = Some(true);
        });
        assert_eq!(plan_backup(BackupVerb::Recover, &p), BackupPlan::Joined);
    }

    #[test]
    fn recover_already_uploading_when_enabled_and_exported() {
        let p = probe(|p| {
            p.exists_on_server = true;
            p.are_enabled = true;
            p.four_s_has_megolm = Some(true);
        });
        assert_eq!(
            plan_backup(BackupVerb::Recover, &p),
            BackupPlan::AlreadyUploading
        );
    }

    #[test]
    fn recover_export_only_when_enabled_but_four_s_missing() {
        let p = probe(|p| {
            p.exists_on_server = true;
            p.are_enabled = true;
            p.four_s_has_megolm = Some(false);
        });
        assert_eq!(plan_backup(BackupVerb::Recover, &p), BackupPlan::ExportOnly);
    }

    #[test]
    fn recover_replace_when_intent_and_our_signature() {
        let p = probe(|p| {
            p.exists_on_server = true;
            p.intent = true;
            p.signed_by_this_device = Some(true);
        });
        assert_eq!(
            plan_backup(BackupVerb::Recover, &p),
            BackupPlan::ReplaceThenEnable
        );
    }

    #[test]
    fn recover_failed_not_ours_never_refuse_join() {
        let p = probe(|p| {
            p.exists_on_server = true;
            p.intent = true;
            p.signed_by_this_device = Some(false);
        });
        assert_eq!(plan_backup(BackupVerb::Recover, &p), BackupPlan::Failed);
        let p = probe(|p| {
            p.exists_on_server = true;
            p.intent = false;
            p.signed_by_this_device = Some(true);
        });
        assert_eq!(plan_backup(BackupVerb::Recover, &p), BackupPlan::Failed);
    }

    #[test]
    fn enable_unverified_refuses() {
        let p = probe(|p| p.verified = false);
        assert_eq!(plan_backup(BackupVerb::Enable, &p), BackupPlan::Unverified);
    }

    #[test]
    fn enable_omit_key_is_kick_only() {
        let p = probe(|p| {
            p.recovery_key_present = false;
            p.are_enabled = true;
        });
        assert_eq!(
            plan_backup(BackupVerb::Enable, &p),
            BackupPlan::AlreadyUploading
        );
        let p = probe(|p| {
            p.recovery_key_present = false;
            p.are_enabled = false;
        });
        assert_eq!(
            plan_backup(BackupVerb::Enable, &p),
            BackupPlan::NeedRecoveryKey
        );
    }

    #[test]
    fn enable_export_only_requires_key() {
        let p = probe(|p| {
            p.are_enabled = true;
            p.four_s_has_megolm = Some(false);
        });
        assert_eq!(plan_backup(BackupVerb::Enable, &p), BackupPlan::ExportOnly);
    }

    #[test]
    fn enable_creates_when_no_hs_backup() {
        assert_eq!(
            plan_backup(BackupVerb::Enable, &probe(|_| {})),
            BackupPlan::EnableAndExport
        );
    }

    #[test]
    fn enable_refuse_join_when_not_ours() {
        let p = probe(|p| {
            p.exists_on_server = true;
            p.signed_by_this_device = Some(false);
        });
        assert_eq!(plan_backup(BackupVerb::Enable, &p), BackupPlan::RefuseJoin);
    }

    #[test]
    fn enable_replace_when_intent_and_our_signature() {
        let p = probe(|p| {
            p.exists_on_server = true;
            p.intent = true;
            p.signed_by_this_device = Some(true);
        });
        assert_eq!(
            plan_backup(BackupVerb::Enable, &p),
            BackupPlan::ReplaceThenEnable
        );
    }

    #[test]
    fn count_is_not_in_the_plan() {
        // Regression: replace is signature + intent, never `count == 0`.
        let p = probe(|p| {
            p.exists_on_server = true;
            p.intent = true;
            p.signed_by_this_device = Some(true);
        });
        assert_eq!(
            plan_backup(BackupVerb::Enable, &p),
            BackupPlan::ReplaceThenEnable
        );
        assert_eq!(
            plan_backup(BackupVerb::Recover, &p),
            BackupPlan::ReplaceThenEnable
        );
    }
}
