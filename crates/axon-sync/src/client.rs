//! Per-account matrix-rust-sdk [`Client`] construction and authentication.
//!
//! Each account gets its own [`Client`] backed by a dedicated SQLite store
//! (under `sync.data_dir/<account_id>`) holding the SDK's state and crypto
//! material — Olm/Megolm sessions, account keys. That store is separate from
//! our Postgres archive and must survive restarts, or we lose historical
//! decryption keys.
//!
//! An account's session is always established at runtime — `login` (fresh
//! password login) or `import_token` (adopt an existing token) — which persist
//! the resulting access token (encrypted) plus device ID. Every later connect
//! restores the session from that stored token; there is no boot-time
//! credential path.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use axon_core::SyncConfig;
use axon_store::{Account, AccountAuthKind, Store, StoredAccountSession};
use matrix_sdk::{
    authentication::{
        matrix::MatrixSession,
        oauth::{ClientId, OAuthSession, UserSession},
    },
    config::RequestConfig,
    cross_process_lock::CrossProcessLockConfig,
    ruma::OwnedUserId,
    store::RoomLoadSettings,
    Client, ClientBuildError, ClientBuilder, SessionMeta, SessionTokens, SqliteStoreConfig,
};

use crate::error::{sdk_err, SyncError};

/// The [`ClientBuilder`] every production client is built from, carrying the
/// settings that must not differ between the first-boot and restore paths.
/// Automatic refresh is enabled only for OAuth restores: legacy Matrix rows
/// retain their pre-ADR 0097 unknown-token behavior exactly, while OAuth access
/// tokens are expected to expire and rotate through their refresh token.
///
/// `SingleProcess` disables the SDK's cross-process store lock. The SDK defaults
/// to `MultiProcess`, which exists for setups that share one crypto store across
/// processes (an iOS app plus its notification-service extension, say); holding
/// it spawns a task that rewrites the `lease_locks` row in
/// `matrix-sdk-crypto.sqlite3` as its own committed transaction every 50ms, for
/// as long as the client lives. Axon is a single process that owns its data dir
/// — it already takes an exclusive Tantivy writer lock there, so a second
/// process on the same directory is unsupported regardless — and on a spinning
/// disk that lease renewal is a continuous stream of tiny fsync'd writes (~43
/// KB/s, ~3.7 GB/day) with the server completely idle.
fn client_builder(homeserver_url: &str, handle_refresh_tokens: bool) -> ClientBuilder {
    let builder = Client::builder()
        .homeserver_url(homeserver_url)
        .cross_process_store_config(CrossProcessLockConfig::SingleProcess);
    if handle_refresh_tokens {
        builder.handle_refresh_tokens()
    } else {
        builder
    }
}

/// Connections each SDK SQLite store keeps in its pool.
///
/// The SDK default is `num_cpus::get_physical() * 4`, which sizes the pool to
/// the *host* rather than to the workload: on a 14-core machine that is 56
/// connections for the crypto store alone, each holding its own page cache (2
/// MiB below), so one store can reach ~112 MiB of cache. Axon is a personal
/// server serving a handful of accounts, and SQLite serialises writers anyway —
/// a deeper pool only buys concurrent *readers*. Eight is what the SDK itself
/// would pick on a dual-core machine, and leaves headroom over the widest
/// concurrent fan-out we issue against a single store (the unread-count sweep's
/// eight-way batch, see `engine::UNREAD_COUNTS_SWEEP_CONCURRENCY`).
const SQLITE_POOL_MAX_SIZE: usize = 8;

/// Cap on each store's WAL residency after a checkpoint, in bytes. This bounds
/// what the journal is *truncated back to*, not its peak — SQLite auto-checkpoints
/// at ~1000 pages (~4 MiB at our 4 KiB page size) regardless.
///
/// The SDK's default is 10 MiB, which for a personal server is out of proportion
/// to the data it fronts: this deployment's crypto store is a 612 KiB database
/// that had accumulated a 3.9 MiB WAL. Tightening to 2 MiB keeps idle on-disk
/// footprint closer to the working set without touching checkpoint frequency.
const SQLITE_JOURNAL_SIZE_LIMIT: u32 = 2 * 1024 * 1024;

/// Store configuration shared by both construction paths.
///
/// Note that `cache_size` is deliberately left at the SDK's 2 MiB default rather
/// than lowered. The SDK bundles a smaller cache into its
/// `with_low_memory_config` preset, but shrinking the page cache trades memory
/// for *more* disk reads — the wrong direction for the spinning-disk and
/// parity-RAID deployments this tuning is aimed at. The win here is bounding the
/// pool, not starving each connection.
fn sqlite_config(data_dir: &Path, store_key: &str) -> SqliteStoreConfig {
    SqliteStoreConfig::new(data_dir)
        .passphrase(Some(store_key))
        .pool_max_size(SQLITE_POOL_MAX_SIZE)
        .journal_size_limit(SQLITE_JOURNAL_SIZE_LIMIT)
}

/// Build a [`Client`] for `account` and restore its stored session, returning
/// the ready-to-sync client. Every account reaching this path already has a
/// persisted session — minted by [`login_new_device`] or
/// [`import_token_new_device`], the only ways an account comes to exist — so
/// there is no first-authentication branch here, only restore.
pub(crate) async fn connect_account(
    store: &Store,
    account: &Account,
    config: &SyncConfig,
) -> Result<Client, SyncError> {
    let store_key = config
        .store_key
        .as_deref()
        .ok_or(SyncError::MissingStoreKey)?;
    let session = store
        .account_session(account.account_id, store_key)
        .await?
        .ok_or_else(|| SyncError::NoCredential(account.user_id.clone()))?;
    let client = restore_account_client(account, config, session).await?;
    tracing::info!(account_id = %account.account_id, user_id = %account.user_id, "restored session");
    Ok(client)
}

/// Build a client against an account's permanent SDK store and restore one
/// already-loaded session onto it.
///
/// Keeping this separate from [`connect_account`] lets tests exercise the
/// filesystem handoff without Postgres while ensuring every production restore
/// uses the same permanent-path construction.
async fn restore_account_client(
    account: &Account,
    config: &SyncConfig,
    session: StoredAccountSession,
) -> Result<Client, SyncError> {
    let store_key = config
        .store_key
        .as_deref()
        .ok_or(SyncError::MissingStoreKey)?;
    let data_dir = config.data_dir.join(account.account_id.to_string());
    create_store_dir(&data_dir).await?;

    let client = client_builder(
        &account.homeserver_url,
        account.auth_kind == AccountAuthKind::OAuth,
    )
    .sqlite_store_with_config_and_cache_path(sqlite_config(&data_dir, store_key), None::<&Path>)
    .build()
    .await
    .map_err(sdk_err)?;
    restore(&client, account, session).await?;
    Ok(client)
}

/// Log `account` in as a **fresh Matrix device** with a password, returning the
/// ready-to-sync client. Used by the runtime login verb (ADR 0022), not the boot
/// path — `connect_account` prefers a stored session, whereas this deliberately
/// starts clean.
///
/// The account's SDK store dir (`data_dir/<account_id>`) is replaced with a fresh
/// one: a reactivated `deactivated` row reuses its `account_id`, and its old
/// Olm/Megolm store would otherwise carry a dead device's keys into a new device
/// session. The old store is **only dropped once login succeeds** — until then it
/// is moved aside and restored on failure, so a rejected password or an
/// unreachable homeserver leaves the account exactly as it was (the durable
/// Postgres archive is never touched here regardless). The new session (device id
/// and access token) is persisted via [`Store::set_account_matrix_session`], so a later
/// restart restores it like any other. The password is consumed here, never stored.
///
/// `account.user_id` must be the full MXID (the login verb resolves identity
/// before minting the row), so it is used directly as the login username.
pub(crate) async fn login_new_device(
    store: &Store,
    account: &Account,
    config: &SyncConfig,
    password: &str,
) -> Result<Client, SyncError> {
    let store_key = config
        .store_key
        .as_deref()
        .ok_or(SyncError::MissingStoreKey)?;

    let data_dir = config.data_dir.join(account.account_id.to_string());
    let backup = config.data_dir.join(format!("{}.prev", account.account_id));

    with_staged_store_dir(&data_dir, &backup, || async {
        let client = client_builder(&account.homeserver_url, false)
            .sqlite_store_with_config_and_cache_path(
                sqlite_config(&data_dir, store_key),
                None::<&Path>,
            )
            .build()
            .await
            .map_err(sdk_err)?;

        let response = client
            .matrix_auth()
            .login_username(&account.user_id, password)
            .initial_device_display_name("axon")
            .send()
            .await
            .map_err(login_err)?;
        store
            .set_account_matrix_session(
                account.account_id,
                response.device_id.as_str(),
                &response.access_token,
                store_key,
            )
            .await?;
        tracing::info!(
            account_id = %account.account_id,
            user_id = %account.user_id,
            device_id = %response.device_id,
            "logged in new device"
        );

        Ok(client)
    })
    .await
}

/// Classify a login failure: a homeserver `M_FORBIDDEN` / `M_UNAUTHORIZED` /
/// `M_USER_DEACTIVATED` means the credentials were rejected
/// ([`SyncError::AuthFailed`] → `401`); anything else (homeserver unreachable, a
/// 5xx, a parse failure) is a transient upstream error ([`SyncError::Sdk`]).
fn login_err(err: matrix_sdk::Error) -> SyncError {
    use matrix_sdk::ruma::api::error::ErrorKind;
    match err.client_api_error_kind() {
        Some(ErrorKind::Forbidden | ErrorKind::Unauthorized | ErrorKind::UserDeactivated) => {
            SyncError::AuthFailed(err.to_string())
        }
        _ => SyncError::Sdk(err.to_string()),
    }
}

/// Classify a `whoami` failure while validating an imported access token: an
/// unknown/revoked token or otherwise-rejected credential is client-actionable
/// ([`SyncError::AuthFailed`] → `401`); anything else (homeserver unreachable, a
/// 5xx, a parse failure) is a transient upstream error ([`SyncError::Sdk`]).
fn whoami_err(err: matrix_sdk::HttpError) -> SyncError {
    use matrix_sdk::ruma::api::error::ErrorKind;
    match err.client_api_error_kind() {
        Some(ErrorKind::UnknownToken(_) | ErrorKind::Forbidden | ErrorKind::Unauthorized) => {
            SyncError::AuthFailed(err.to_string())
        }
        _ => SyncError::Sdk(err.to_string()),
    }
}

/// Adopt an existing Matrix `access_token` + `device_id` as a **fresh device
/// session** for `account`, returning the ready-to-sync client. Used by the
/// runtime token-import verb — the capability the retired
/// `sync.account.access_token` boot path carried, closing the gap that
/// retiring config-based provisioning would otherwise have left (GH #65/#66).
///
/// Unlike [`login_new_device`], there is no homeserver call that confirms the
/// token up front (session restore is a purely local operation), so this
/// validates it explicitly with a `whoami` round-trip before persisting
/// anything: the response's `user_id` (and `device_id`, if the homeserver
/// reports one) must match what the caller supplied, so a mismatched or
/// revoked token is rejected here rather than silently accepted and only
/// discovered on the first sync.
///
/// The account's SDK store dir is replaced with a fresh one exactly like
/// [`login_new_device`] (a reactivated `deactivated` row must not carry a dead
/// device's Olm/Megolm keys into the imported session), staged the same
/// crash-safe way: the prior store is dropped only once the token is
/// confirmed valid and the session persisted.
pub(crate) async fn import_token_new_device(
    store: &Store,
    account: &Account,
    config: &SyncConfig,
    access_token: &str,
    device_id: &str,
) -> Result<Client, SyncError> {
    let store_key = config
        .store_key
        .as_deref()
        .ok_or(SyncError::MissingStoreKey)?;

    let data_dir = config.data_dir.join(account.account_id.to_string());
    let backup = config.data_dir.join(format!("{}.prev", account.account_id));

    with_staged_store_dir(&data_dir, &backup, || async {
        let client = client_builder(&account.homeserver_url, false)
            .sqlite_store_with_config_and_cache_path(
                sqlite_config(&data_dir, store_key),
                None::<&Path>,
            )
            .build()
            .await
            .map_err(sdk_err)?;

        // `restore` reads the device id off the `Account` it's given (not the
        // DB row, which has none yet for a fresh identity) — supply it via a
        // throwaway copy rather than mutating the caller's row.
        let mut with_device = account.clone();
        with_device.device_id = Some(device_id.to_owned());
        restore(
            &client,
            &with_device,
            StoredAccountSession::Matrix {
                access_token: access_token.to_owned(),
            },
        )
        .await?;

        let who = client.whoami().await.map_err(whoami_err)?;
        if who.user_id.as_str() != account.user_id {
            return Err(SyncError::AuthFailed(format!(
                "access token belongs to {}, not {}",
                who.user_id, account.user_id
            )));
        }
        if let Some(reported) = who.device_id.as_ref() {
            if reported.as_str() != device_id {
                return Err(SyncError::AuthFailed(format!(
                    "access token belongs to device {reported}, not {device_id}"
                )));
            }
        }

        store
            .set_account_matrix_session(account.account_id, device_id, access_token, store_key)
            .await?;
        tracing::info!(
            account_id = %account.account_id,
            user_id = %account.user_id,
            device_id,
            "imported existing access token as a new device"
        );

        Ok(client)
    })
    .await
}

/// Restore a Matrix or OAuth session onto `client` from encrypted storage.
/// Requires the account row to carry the `device_id` the tokens belong to.
pub(crate) async fn restore(
    client: &Client,
    account: &Account,
    session: StoredAccountSession,
) -> Result<(), SyncError> {
    let device_id = account
        .device_id
        .clone()
        .ok_or_else(|| SyncError::MissingDeviceId(account.user_id.clone()))?;
    let user_id = OwnedUserId::try_from(account.user_id.as_str()).map_err(sdk_err)?;

    let meta = SessionMeta {
        user_id,
        device_id: device_id.as_str().into(),
    };

    match session {
        StoredAccountSession::Matrix { access_token } => client
            .matrix_auth()
            .restore_session(
                MatrixSession {
                    meta,
                    tokens: SessionTokens {
                        access_token,
                        refresh_token: None,
                    },
                },
                RoomLoadSettings::default(),
            )
            .await
            .map_err(sdk_err),
        StoredAccountSession::OAuth {
            access_token,
            refresh_token,
            client_id,
        } => client
            .oauth()
            .restore_session(
                OAuthSession {
                    client_id: ClientId::new(client_id),
                    user: UserSession {
                        meta,
                        tokens: SessionTokens {
                            access_token,
                            refresh_token: Some(refresh_token),
                        },
                    },
                },
                RoomLoadSettings::default(),
            )
            .await
            .map_err(sdk_err),
    }
}

/// Create the SDK store directory (and parents) if it doesn't exist.
async fn create_store_dir(path: &Path) -> Result<(), SyncError> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|e| SyncError::Sdk(format!("creating SDK store dir {}: {e}", path.display())))
}

/// Directory containing pre-account Matrix OAuth QR-login SDK stores.
/// Individual directory names are generated UUIDs and are also persisted in a
/// non-secret Postgres breadcrumb before this path is touched.
pub(crate) fn matrix_oauth_acquire_root(config: &SyncConfig) -> PathBuf {
    config.data_dir.join(".matrix-oauth-acquire")
}

/// Resolve one generated staging name without accepting path traversal from a
/// corrupted database row.
pub(crate) fn matrix_oauth_acquire_staging_dir(
    config: &SyncConfig,
    staging_dir_name: &str,
) -> Result<PathBuf, SyncError> {
    uuid::Uuid::parse_str(staging_dir_name).map_err(|_| {
        SyncError::Sdk("invalid Matrix OAuth acquire staging directory name".to_owned())
    })?;
    Ok(matrix_oauth_acquire_root(config).join(staging_dir_name))
}

/// Build the isolated OAuth client used while Axon has no account row yet.
/// Every SDK request receives the same bounded timeout as Matrix OAuth
/// discovery/registration; the overall interactive flow has a separate TTL.
#[derive(Debug)]
pub(crate) enum MatrixOAuthAcquireClientError {
    Configuration,
    LocalStorage,
    Upstream,
}

pub(crate) async fn build_matrix_oauth_acquire_client(
    config: &SyncConfig,
    staging_dir_name: &str,
    server_name_or_url: &str,
) -> Result<Client, MatrixOAuthAcquireClientError> {
    let store_key = config
        .store_key
        .as_deref()
        .ok_or(MatrixOAuthAcquireClientError::Configuration)?;
    let staging = matrix_oauth_acquire_staging_dir(config, staging_dir_name)
        .map_err(|_| MatrixOAuthAcquireClientError::Configuration)?;
    remove_dir_if_present(&staging)
        .await
        .map_err(|_| MatrixOAuthAcquireClientError::LocalStorage)?;
    create_store_dir(&staging)
        .await
        .map_err(|_| MatrixOAuthAcquireClientError::LocalStorage)?;
    let timeout = Duration::from_secs(config.matrix_oauth.request_timeout_secs);
    Client::builder()
        .server_name_or_homeserver_url(server_name_or_url)
        .handle_refresh_tokens()
        .cross_process_store_config(CrossProcessLockConfig::SingleProcess)
        .request_config(RequestConfig::new().timeout(timeout))
        .sqlite_store_with_config_and_cache_path(sqlite_config(&staging, store_key), None::<&Path>)
        .build()
        .await
        .map_err(|error| match error {
            ClientBuildError::AutoDiscovery(_)
            | ClientBuildError::SlidingSyncVersion(_)
            | ClientBuildError::Http(_) => MatrixOAuthAcquireClientError::Upstream,
            ClientBuildError::MissingHomeserver
            | ClientBuildError::InvalidServerName
            | ClientBuildError::Url(_) => MatrixOAuthAcquireClientError::Configuration,
            ClientBuildError::SqliteStore(_) => MatrixOAuthAcquireClientError::LocalStorage,
        })
}

/// Remove an abandoned pre-account SDK store. Idempotent for cancellation,
/// failed login, and boot reconciliation.
pub(crate) async fn remove_matrix_oauth_acquire_staging(
    config: &SyncConfig,
    staging_dir_name: &str,
) -> Result<(), SyncError> {
    let staging = matrix_oauth_acquire_staging_dir(config, staging_dir_name)?;
    remove_dir_if_present(&staging).await
}

/// The retained account store displaced by an adopted Matrix OAuth login.
///
/// This is keyed by account rather than flow so a successful cold connect can
/// finish cleanup after the flow breadcrumb has been consumed. Keeping it
/// under the acquire root also avoids colliding with the ordinary login
/// staging path at `data_dir/<account_id>.prev`.
fn matrix_oauth_acquire_previous_dir(config: &SyncConfig, account_id: uuid::Uuid) -> PathBuf {
    matrix_oauth_acquire_root(config).join(format!("{account_id}.previous"))
}

/// Remove the retained pre-adoption SDK store after the adopted store has
/// opened successfully. Idempotent so a later account connection can retry a
/// cleanup interrupted by a crash or filesystem error.
pub(crate) async fn finish_matrix_oauth_acquire_adoption(
    config: &SyncConfig,
    account_id: uuid::Uuid,
) -> Result<(), SyncError> {
    remove_dir_if_present(&matrix_oauth_acquire_previous_dir(config, account_id)).await
}

/// Adopt a completed QR login's SDK store as an account's permanent store.
///
/// The staging store is authoritative once the encrypted OAuth session has
/// committed. Moving the previous permanent store aside and then renaming the
/// staging directory is restart-idempotent: a crash between either rename is
/// completed from the same breadcrumb on the next boot. The displaced store
/// remains available until a client has successfully opened the adopted store;
/// [`finish_matrix_oauth_acquire_adoption`] owns its later cleanup.
pub(crate) async fn adopt_matrix_oauth_acquire_staging(
    config: &SyncConfig,
    staging_dir_name: &str,
    account_id: uuid::Uuid,
) -> Result<(), SyncError> {
    let root = matrix_oauth_acquire_root(config);
    let staging = matrix_oauth_acquire_staging_dir(config, staging_dir_name)?;
    let permanent = config.data_dir.join(account_id.to_string());
    let previous = matrix_oauth_acquire_previous_dir(config, account_id);
    create_store_dir(&root).await?;

    let staging_exists = tokio::fs::try_exists(&staging)
        .await
        .map_err(|e| SyncError::Sdk(format!("checking Matrix OAuth staging store: {e}")))?;
    if staging_exists {
        let previous_exists = tokio::fs::try_exists(&previous)
            .await
            .map_err(|e| SyncError::Sdk(format!("checking prior SDK store backup: {e}")))?;
        if !previous_exists {
            match tokio::fs::rename(&permanent, &previous).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(SyncError::Sdk(format!(
                        "staging previous SDK store for Matrix OAuth adoption: {e}"
                    )))
                }
            }
        } else {
            // A surviving backup proves an earlier attempt already moved the old
            // store. Any permanent directory alongside the still-present staging
            // directory is an incomplete new copy and is safe to discard.
            remove_dir_if_present(&permanent).await?;
        }
        tokio::fs::rename(&staging, &permanent)
            .await
            .map_err(|e| SyncError::Sdk(format!("adopting Matrix OAuth SDK store: {e}")))?;
    } else if !tokio::fs::try_exists(&permanent)
        .await
        .map_err(|e| SyncError::Sdk(format!("checking adopted SDK store: {e}")))?
    {
        return Err(SyncError::Sdk(
            "Matrix OAuth SDK staging and permanent stores are both missing".to_owned(),
        ));
    }

    Ok(())
}

/// `remove_dir_all` that treats an absent directory as success.
///
/// Retries on EMFILE (os error 24) with exponential back-off: the Matrix SDK
/// spawns internal tasks that hold `Arc<Client>` (and thus open SQLite fds)
/// even after the main sync task has been reaped.  Those tasks typically release
/// their last Arc clone within a few hundred milliseconds, so retrying allows
/// `remove_dir_all` to succeed once the fds are freed.
async fn remove_dir_if_present(path: &Path) -> Result<(), SyncError> {
    const EMFILE: i32 = 24;
    let mut delay_ms = 100u64;
    for attempt in 0..6 {
        match tokio::fs::remove_dir_all(path).await {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if e.raw_os_error() == Some(EMFILE) && attempt < 5 => {
                tracing::debug!(
                    path = %path.display(),
                    attempt,
                    delay_ms,
                    "remove_dir_all hit EMFILE; retrying after delay"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                delay_ms *= 2;
            }
            Err(e) => {
                return Err(SyncError::Sdk(format!(
                    "removing dir {}: {e}",
                    path.display()
                )))
            }
        }
    }
    Err(SyncError::Sdk(format!(
        "removing dir {}: EMFILE after 6 attempts",
        path.display()
    )))
}

/// Remove an account's on-disk SDK stores: the live directory, the ordinary
/// login staging backup, and the retained pre-QR-adoption store. Used by the
/// account-delete teardown so every store containing this account's crypto
/// state is removed before its database row. Idempotent — absent directories
/// are success — so a delete retry or boot reconciliation can re-run it.
pub(crate) async fn remove_account_store_dirs(
    config: &SyncConfig,
    account_id: uuid::Uuid,
) -> Result<(), SyncError> {
    let data_dir = config.data_dir.join(account_id.to_string());
    let backup = config.data_dir.join(format!("{account_id}.prev"));
    let oauth_acquire_backup = matrix_oauth_acquire_previous_dir(config, account_id);
    remove_dir_if_present(&data_dir).await?;
    remove_dir_if_present(&backup).await?;
    remove_dir_if_present(&oauth_acquire_backup).await?;
    Ok(())
}

/// Run a fresh-device login (`build`) against an empty store at `data_dir`,
/// preserving any existing store until the login is known to have succeeded.
///
/// Staging: move an existing `data_dir` aside to `backup`, create an empty
/// `data_dir`, run `build`. On success the old store (`backup`) is dropped — the
/// fresh device's store is authoritative. On failure the partial fresh store is
/// removed and the old one is moved back, so a rejected/failed login has no side
/// effect on the account.
///
/// **Crash recovery:** the old store is dropped *only* after a successful login,
/// so a `backup` left by an interrupted prior attempt is the only surviving copy
/// of the prior store. It is therefore treated as authoritative and restored (not
/// deleted) at the start: any concurrent `data_dir` (an uncommitted/partial fresh
/// store) is discarded and the backup moved back, before the normal stage begins.
/// Every step is idempotent, so repeated crashes converge rather than lose data.
async fn with_staged_store_dir<F, Fut, T>(
    data_dir: &Path,
    backup: &Path,
    build: F,
) -> Result<T, SyncError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, SyncError>>,
{
    // Recover from an interrupted prior attempt: a surviving `backup` outranks any
    // `data_dir` (which may be a half-built fresh store), so restore it first
    // rather than deleting it — otherwise an interrupted login could discard the
    // prior store even though no fresh login ever succeeded.
    let backup_present = tokio::fs::try_exists(backup)
        .await
        .map_err(|e| SyncError::Sdk(format!("checking {}: {e}", backup.display())))?;
    if backup_present {
        remove_dir_if_present(data_dir).await?;
        tokio::fs::rename(backup, data_dir).await.map_err(|e| {
            SyncError::Sdk(format!(
                "restoring staged SDK store {}: {e}",
                backup.display()
            ))
        })?;
    }

    // Move the current store aside (a no-op on the common first-login case).
    match tokio::fs::rename(data_dir, backup).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(SyncError::Sdk(format!(
                "staging SDK store dir {}: {e}",
                data_dir.display()
            )))
        }
    }
    create_store_dir(data_dir).await?;

    match build().await {
        Ok(value) => {
            // Login succeeded: the fresh store stands; drop the old one (best-effort
            // — a leftover backup is harmless and reclaimed on the next attempt).
            let _ = remove_dir_if_present(backup).await;
            Ok(value)
        }
        Err(err) => {
            // Roll back: discard the partial fresh store, restore the prior one.
            let _ = remove_dir_if_present(data_dir).await;
            let _ = tokio::fs::rename(backup, data_dir).await;
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        adopt_matrix_oauth_acquire_staging, client_builder, finish_matrix_oauth_acquire_adoption,
        matrix_oauth_acquire_previous_dir, matrix_oauth_acquire_root, restore,
        restore_account_client, sqlite_config, with_staged_store_dir,
    };
    use crate::error::SyncError;
    use axon_core::SyncConfig;
    use axon_store::{Account, AccountAuthKind, AccountState, StoredAccountSession};
    use chrono::Utc;
    use std::path::PathBuf;
    use uuid::Uuid;

    /// A throwaway directory under the OS temp dir, removed on drop.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("axon-stage-test-{}", uuid::Uuid::new_v4()));
            TempRoot(p)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn account(auth_kind: AccountAuthKind) -> Account {
        Account {
            account_id: Uuid::new_v4(),
            user_id: "@alice:example.org".to_owned(),
            homeserver_url: "https://example.org/".to_owned(),
            device_id: Some("AXONDEVICE".to_owned()),
            auth_kind,
            state: AccountState::Active,
            verified: false,
            backup_enable_intent: false,
            sync_token: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn sync_config(root: &TempRoot) -> SyncConfig {
        SyncConfig {
            data_dir: root.0.clone(),
            store_key: Some("test-store-key".to_owned()),
            ..SyncConfig::default()
        }
    }

    #[tokio::test]
    async fn oauth_adoption_replaces_previous_store_and_is_idempotent() {
        let root = TempRoot::new();
        let config = sync_config(&root);
        let flow_id = Uuid::new_v4();
        let staging_name = flow_id.to_string();
        let account_id = Uuid::new_v4();
        let staging = matrix_oauth_acquire_root(&config).join(&staging_name);
        let permanent = config.data_dir.join(account_id.to_string());
        let previous = matrix_oauth_acquire_previous_dir(&config, account_id);
        tokio::fs::create_dir_all(&staging).await.unwrap();
        tokio::fs::create_dir_all(&permanent).await.unwrap();
        tokio::fs::write(staging.join("new-store"), b"new")
            .await
            .unwrap();
        tokio::fs::write(permanent.join("old-store"), b"old")
            .await
            .unwrap();

        adopt_matrix_oauth_acquire_staging(&config, &staging_name, account_id)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read(permanent.join("new-store")).await.unwrap(),
            b"new"
        );
        assert!(!permanent.join("old-store").exists());
        assert!(!staging.exists());
        assert_eq!(
            tokio::fs::read(previous.join("old-store")).await.unwrap(),
            b"old"
        );

        adopt_matrix_oauth_acquire_staging(&config, &staging_name, account_id)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read(permanent.join("new-store")).await.unwrap(),
            b"new"
        );
        assert!(previous.join("old-store").exists());

        finish_matrix_oauth_acquire_adoption(&config, account_id)
            .await
            .unwrap();
        assert!(!previous.exists());
        finish_matrix_oauth_acquire_adoption(&config, account_id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn oauth_adoption_resumes_between_the_two_renames() {
        let root = TempRoot::new();
        let config = sync_config(&root);
        let flow_id = Uuid::new_v4();
        let staging_name = flow_id.to_string();
        let account_id = Uuid::new_v4();
        let acquire_root = matrix_oauth_acquire_root(&config);
        let staging = acquire_root.join(&staging_name);
        let previous = matrix_oauth_acquire_previous_dir(&config, account_id);
        let permanent = config.data_dir.join(account_id.to_string());
        tokio::fs::create_dir_all(&staging).await.unwrap();
        tokio::fs::create_dir_all(&previous).await.unwrap();
        tokio::fs::write(staging.join("new-store"), b"new")
            .await
            .unwrap();
        tokio::fs::write(previous.join("old-store"), b"old")
            .await
            .unwrap();

        adopt_matrix_oauth_acquire_staging(&config, &staging_name, account_id)
            .await
            .unwrap();
        assert!(permanent.join("new-store").exists());
        assert!(previous.join("old-store").exists());
        assert!(!staging.exists());
    }

    #[tokio::test]
    async fn oauth_adoption_reopens_the_store_at_its_permanent_path() {
        let root = TempRoot::new();
        let config = sync_config(&root);
        let staging_name = Uuid::new_v4().to_string();
        let account = account(AccountAuthKind::OAuth);
        let staging = matrix_oauth_acquire_root(&config).join(&staging_name);
        let permanent = config.data_dir.join(account.account_id.to_string());
        let previous = matrix_oauth_acquire_previous_dir(&config, account.account_id);
        tokio::fs::create_dir_all(&staging).await.unwrap();
        tokio::fs::create_dir_all(&permanent).await.unwrap();
        tokio::fs::write(permanent.join("old-store"), b"old")
            .await
            .unwrap();

        let acquisition_client = client_builder(&account.homeserver_url, true)
            .sqlite_store_with_config_and_cache_path(
                sqlite_config(&staging, config.store_key.as_deref().unwrap()),
                None::<&std::path::Path>,
            )
            .build()
            .await
            .unwrap();
        restore(
            &acquisition_client,
            &account,
            StoredAccountSession::OAuth {
                access_token: "oauth-access".to_owned(),
                refresh_token: "oauth-refresh".to_owned(),
                client_id: "axon-public-client".to_owned(),
            },
        )
        .await
        .unwrap();
        drop(acquisition_client);

        adopt_matrix_oauth_acquire_staging(&config, &staging_name, account.account_id)
            .await
            .unwrap();
        let permanent_client = restore_account_client(
            &account,
            &config,
            StoredAccountSession::OAuth {
                access_token: "oauth-access".to_owned(),
                refresh_token: "oauth-refresh".to_owned(),
                client_id: "axon-public-client".to_owned(),
            },
        )
        .await
        .unwrap();

        permanent_client
            .state_store()
            .get_room_infos(&matrix_sdk::store::RoomLoadSettings::default())
            .await
            .expect("the promoted state store must be readable");
        assert!(previous.join("old-store").exists());
        finish_matrix_oauth_acquire_adoption(&config, account.account_id)
            .await
            .unwrap();
        assert!(!previous.exists());
        assert!(!staging.exists(), "the old staging path must stay absent");
        assert!(config
            .data_dir
            .join(account.account_id.to_string())
            .join("matrix-sdk-state.sqlite3")
            .exists());
    }

    #[tokio::test]
    async fn oauth_adoption_keeps_previous_store_when_reopen_fails() {
        let root = TempRoot::new();
        let config = sync_config(&root);
        let staging_name = Uuid::new_v4().to_string();
        let account = account(AccountAuthKind::OAuth);
        let staging = matrix_oauth_acquire_root(&config).join(&staging_name);
        let permanent = config.data_dir.join(account.account_id.to_string());
        let previous = matrix_oauth_acquire_previous_dir(&config, account.account_id);
        tokio::fs::create_dir_all(&staging).await.unwrap();
        tokio::fs::create_dir_all(&permanent).await.unwrap();
        tokio::fs::write(staging.join("matrix-sdk-state.sqlite3"), b"not sqlite")
            .await
            .unwrap();
        tokio::fs::write(permanent.join("old-store"), b"old")
            .await
            .unwrap();

        adopt_matrix_oauth_acquire_staging(&config, &staging_name, account.account_id)
            .await
            .unwrap();
        assert!(restore_account_client(
            &account,
            &config,
            StoredAccountSession::OAuth {
                access_token: "oauth-access".to_owned(),
                refresh_token: "oauth-refresh".to_owned(),
                client_id: "axon-public-client".to_owned(),
            },
        )
        .await
        .is_err());
        assert_eq!(
            tokio::fs::read(previous.join("old-store")).await.unwrap(),
            b"old"
        );
    }

    #[tokio::test]
    async fn restores_legacy_matrix_session_without_refresh_token() {
        let client = client_builder("https://example.org/", false)
            .build()
            .await
            .unwrap();
        restore(
            &client,
            &account(AccountAuthKind::Matrix),
            StoredAccountSession::Matrix {
                access_token: "matrix-access".to_owned(),
            },
        )
        .await
        .unwrap();

        let session = client.matrix_auth().session().expect("Matrix session");
        assert_eq!(session.tokens.access_token, "matrix-access");
        assert!(session.tokens.refresh_token.is_none());
    }

    #[tokio::test]
    async fn restores_oauth_session_with_rotating_refresh_state() {
        let client = client_builder("https://example.org/", true)
            .build()
            .await
            .unwrap();
        restore(
            &client,
            &account(AccountAuthKind::OAuth),
            StoredAccountSession::OAuth {
                access_token: "oauth-access".to_owned(),
                refresh_token: "oauth-refresh".to_owned(),
                client_id: "axon-public-client".to_owned(),
            },
        )
        .await
        .unwrap();

        let session = client.oauth().full_session().expect("OAuth session");
        assert_eq!(session.client_id.as_str(), "axon-public-client");
        assert_eq!(session.user.tokens.access_token, "oauth-access");
        assert_eq!(
            session.user.tokens.refresh_token.as_deref(),
            Some("oauth-refresh")
        );
    }

    /// A failed login must leave the prior store untouched — Codex's concern: a
    /// rejected password should not destroy a reactivating account's old store.
    #[tokio::test]
    async fn failed_login_restores_prior_store() {
        let root = TempRoot::new();
        let data_dir = root.0.join("acct");
        let backup = root.0.join("acct.prev");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        tokio::fs::write(data_dir.join("crypto.sqlite"), b"old-keys")
            .await
            .unwrap();

        let res: Result<(), SyncError> = with_staged_store_dir(&data_dir, &backup, || async {
            // The fresh store may have been partially built before login failed.
            tokio::fs::write(data_dir.join("partial"), b"x")
                .await
                .unwrap();
            Err(SyncError::AuthFailed("bad password".into()))
        })
        .await;

        assert!(matches!(res, Err(SyncError::AuthFailed(_))));
        // Prior store restored verbatim; the partial fresh store and backup are gone.
        assert_eq!(
            tokio::fs::read(data_dir.join("crypto.sqlite"))
                .await
                .unwrap(),
            b"old-keys"
        );
        assert!(!tokio::fs::try_exists(data_dir.join("partial"))
            .await
            .unwrap());
        assert!(!tokio::fs::try_exists(&backup).await.unwrap());
    }

    /// A successful login keeps the fresh store and drops the old one.
    #[tokio::test]
    async fn successful_login_drops_old_store() {
        let root = TempRoot::new();
        let data_dir = root.0.join("acct");
        let backup = root.0.join("acct.prev");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        tokio::fs::write(data_dir.join("old-keys"), b"dead-device")
            .await
            .unwrap();

        let res: Result<(), SyncError> = with_staged_store_dir(&data_dir, &backup, || async {
            tokio::fs::write(data_dir.join("new-keys"), b"fresh")
                .await
                .unwrap();
            Ok(())
        })
        .await;

        assert!(res.is_ok());
        // Fresh store kept; the dead device's store and the backup are gone.
        assert!(tokio::fs::try_exists(data_dir.join("new-keys"))
            .await
            .unwrap());
        assert!(!tokio::fs::try_exists(data_dir.join("old-keys"))
            .await
            .unwrap());
        assert!(!tokio::fs::try_exists(&backup).await.unwrap());
    }

    /// A `backup` left by a crash mid-stage (store moved aside, never restored or
    /// committed) is recovered, not deleted — even when the recovering attempt
    /// itself fails. Codex P2: an interrupted login must not discard the prior
    /// store's only surviving copy.
    #[tokio::test]
    async fn recovers_orphaned_backup_from_interrupted_stage() {
        let root = TempRoot::new();
        let data_dir = root.0.join("acct");
        let backup = root.0.join("acct.prev");
        // Simulate the crash window: backup holds the real store, data_dir is gone.
        tokio::fs::create_dir_all(&backup).await.unwrap();
        tokio::fs::write(backup.join("crypto.sqlite"), b"old-keys")
            .await
            .unwrap();

        // Even a *failing* recovering attempt must end with the store intact.
        let res: Result<(), SyncError> = with_staged_store_dir(&data_dir, &backup, || async {
            Err(SyncError::AuthFailed("bad password".into()))
        })
        .await;

        assert!(matches!(res, Err(SyncError::AuthFailed(_))));
        assert_eq!(
            tokio::fs::read(data_dir.join("crypto.sqlite"))
                .await
                .unwrap(),
            b"old-keys"
        );
        assert!(!tokio::fs::try_exists(&backup).await.unwrap());
    }

    /// First-ever login (no existing store) works and leaves no backup behind.
    #[tokio::test]
    async fn first_login_with_no_prior_store() {
        let root = TempRoot::new();
        let data_dir = root.0.join("acct");
        let backup = root.0.join("acct.prev");

        let res: Result<(), SyncError> = with_staged_store_dir(&data_dir, &backup, || async {
            tokio::fs::write(data_dir.join("keys"), b"fresh")
                .await
                .unwrap();
            Ok(())
        })
        .await;

        assert!(res.is_ok());
        assert!(tokio::fs::try_exists(data_dir.join("keys")).await.unwrap());
        assert!(!tokio::fs::try_exists(&backup).await.unwrap());
    }
}
