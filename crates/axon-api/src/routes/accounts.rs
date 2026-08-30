//! Account read endpoints: list the accounts this Axon manages, and read one.
//!
//! These are pure store reads (no secrets — the access token is never exposed).
//! Like every `/v1/` route — the reads here and the destructive/secret-bearing
//! lifecycle verbs alike — they sit behind the bearer-token gate (M7b, ADR 0029).

use std::sync::Arc;

use axon_store::{Account, Store};
use axum::extract::State;
use uuid::Uuid;

use crate::backup_state::BackupStateProvider;
use crate::dto::{
    AccountDto, EnableBackupRequest, EnableBackupResponseDto, ImportTokenRequest, LoginRequest,
    RecoverRequest, RecoverResponseDto, RedecryptUtdsResponse,
};
use crate::extract::{Json, Path};
use crate::lifecycle::AccountLifecycle;
use crate::response::{ApiError, ApiResponse};
use crate::sync_state::SyncStateProvider;

/// Live `AccountDto` for a stored row: sync-state plus a bounded backup snapshot.
async fn account_dto(
    account: Account,
    sync_state: &dyn SyncStateProvider,
    backup_state: &dyn BackupStateProvider,
) -> AccountDto {
    let id = account.account_id;
    let state = sync_state.sync_state(id).into();
    let backup = backup_state.snapshot(id).await;
    AccountDto::from_account(account, state, backup)
}

/// List the accounts this Axon manages, oldest first — the **client-visible**
/// set: `active` and `deactivated`.
///
/// Logged-out (`deactivated`) accounts are included so a client that has lost the
/// `account_id` can still discover one to offer re-login (the login verb both
/// produces and reactivates them). The transient `deleting` teardown state is
/// excluded — a row mid-removal isn't something to act on — but any account, in
/// any state, can still be read by id via [`get_account`].
#[utoipa::path(
    get,
    path = "/v1/accounts",
    responses(
        (status = 200, description = "Client-visible accounts (active + deactivated), oldest first", body = ApiResponse<Vec<AccountDto>>),
    ),
    tag = "accounts",
)]
pub async fn list_accounts(
    State(store): State<Store>,
    State(sync_state): State<Arc<dyn SyncStateProvider>>,
    State(backup_state): State<Arc<dyn BackupStateProvider>>,
) -> Result<ApiResponse<Vec<AccountDto>>, ApiError> {
    let accounts = store.list_client_visible_accounts().await?;
    // Fan out backup probes so a hung account cannot pin the whole list
    // (ADR 0098). Each probe is bounded inside the provider.
    let dtos = futures_util::future::join_all(accounts.into_iter().map(|a| {
        let sync_state = Arc::clone(&sync_state);
        let backup_state = Arc::clone(&backup_state);
        async move { account_dto(a, sync_state.as_ref(), backup_state.as_ref()).await }
    }))
    .await;
    Ok(ApiResponse::new(dtos))
}

/// Add or reactivate a Matrix account at runtime, then return the resulting
/// active account. Idempotent by Matrix `username`: a new identity is minted, a
/// logged-out (`deactivated`) account is reactivated using its stored endpoint,
/// and an already-`active` account is returned **unchanged** (a no-op — the desired end
/// state already holds, so the password isn't consulted and nothing is touched).
/// An account mid-deletion (`deleting`) is a `409`. `username` must be a full
/// Matrix user ID (a malformed one is a `400`); bad credentials are a `401`. The
/// password is used once and never stored.
///
/// `homeserver_url` is optional: when omitted, the server discovers the
/// canonical homeserver from the user ID's server name (`.well-known/matrix/client`,
/// falling back to the server name itself) — so clients need only username +
/// password. The URL is a connection endpoint; the Matrix ID keys identity.
/// A failed discovery is a `502`. The `username` domain is then checked against
/// the homeserver's own declared server name (best-effort): a user ID written
/// with the homeserver's hostname (`@adam:matrix.example.org`) is a `400` whose
/// message suggests the ID they almost certainly meant (`@adam:example.org`),
/// rather than a misleading `401` — and never a silent login as a different
/// identity.
///
/// Secret-bearing; gated by the bearer-token auth layer like every `/v1/` route
/// (M7b, ADR 0029).
#[utoipa::path(
    post,
    path = "/v1/accounts/login",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "The active account (newly logged in, reactivated, or already active)", body = ApiResponse<AccountDto>),
        (status = 400, description = "Malformed request (e.g. invalid user ID, or a user ID written with the homeserver's hostname — the message suggests the canonical spelling)", body = crate::response::ErrorResponse),
        (status = 401, description = "Either the bearer gate rejected the request before the handler ran (missing, malformed, or revoked token — carries a WWW-Authenticate: Bearer challenge), or the request was authorized but the Matrix homeserver rejected the supplied credentials (post-auth — no challenge).", body = crate::response::ErrorResponse, headers(
            ("WWW-Authenticate" = String, description = "RFC 6750 bearer challenge, present only on a gate rejection: `Bearer` for a missing/malformed credential, `Bearer error=\"invalid_token\"` for an unknown or revoked token. Absent when the 401 is the homeserver rejecting the supplied Matrix credentials."),
        )),
        (status = 409, description = "The account is being deleted", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error (including failed homeserver discovery)", body = crate::response::ErrorResponse),
    ),
    tag = "accounts",
)]
pub async fn login(
    State(store): State<Store>,
    State(lifecycle): State<Arc<dyn AccountLifecycle>>,
    State(sync_state): State<Arc<dyn SyncStateProvider>>,
    State(backup_state): State<Arc<dyn BackupStateProvider>>,
    Json(req): Json<LoginRequest>,
) -> Result<ApiResponse<AccountDto>, ApiError> {
    let account_id = lifecycle
        .login(req.homeserver_url.as_deref(), &req.username, &req.password)
        .await?;
    // Read the row back so the response reflects the persisted state (device id,
    // timestamps) rather than re-deriving it. It was just made active, so a
    // missing row here is a real internal inconsistency.
    let account = store
        .get_account(account_id)
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok(ApiResponse::new(
        account_dto(account, sync_state.as_ref(), backup_state.as_ref()).await,
    ))
}

/// Adopt an existing Matrix access token as a runtime account, then return the
/// resulting active account — the runtime replacement for the retired
/// `sync.account.access_token` boot-provisioning path: restore a session axon
/// didn't mint itself (issued by another client, or an SSO-only account with no
/// password) without a fresh login. Idempotent by Matrix `username`, exactly
/// like [`login`]: a new identity mints a row, a logged-out (`deactivated`)
/// account is reactivated using its *stored* endpoint (the request's
/// `homeserver_url` is only consulted for a new identity), and an already-`active`
/// account is returned unchanged (the token isn't consulted). An account
/// mid-deletion (`deleting`) is a `409`.
///
/// Unlike `login`, no homeserver call confirms the token before this returns —
/// session restore is a purely local SDK operation — so the token (and, if the
/// homeserver reports one, the device id) is validated with a `whoami`
/// round-trip before the account is activated; a mismatched or revoked token is
/// a `401`, same as `login`'s rejected-credential case.
///
/// Secret-bearing; gated by the bearer-token auth layer like every `/v1/` route
/// (M7b, ADR 0029).
#[utoipa::path(
    post,
    path = "/v1/accounts/import",
    request_body = ImportTokenRequest,
    responses(
        (status = 200, description = "The active account (newly imported, reactivated, or already active)", body = ApiResponse<AccountDto>),
        (status = 400, description = "Malformed request (e.g. invalid user ID)", body = crate::response::ErrorResponse),
        (status = 401, description = "Either the bearer gate rejected the request before the handler ran (missing, malformed, or revoked token — carries a WWW-Authenticate: Bearer challenge), or the request was authorized but the supplied access token was rejected or belongs to a different user/device (post-auth — no challenge).", body = crate::response::ErrorResponse, headers(
            ("WWW-Authenticate" = String, description = "RFC 6750 bearer challenge, present only on a gate rejection: `Bearer` for a missing/malformed credential, `Bearer error=\"invalid_token\"` for an unknown or revoked token. Absent when the 401 is the homeserver rejecting the supplied access token."),
        )),
        (status = 409, description = "The account is being deleted", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream homeserver error", body = crate::response::ErrorResponse),
    ),
    tag = "accounts",
)]
pub async fn import_token(
    State(store): State<Store>,
    State(lifecycle): State<Arc<dyn AccountLifecycle>>,
    State(sync_state): State<Arc<dyn SyncStateProvider>>,
    State(backup_state): State<Arc<dyn BackupStateProvider>>,
    Json(req): Json<ImportTokenRequest>,
) -> Result<ApiResponse<AccountDto>, ApiError> {
    let account_id = lifecycle
        .import_token(
            &req.homeserver_url,
            &req.username,
            &req.access_token,
            &req.device_id,
        )
        .await?;
    // Read the row back so the response reflects the persisted state (device id,
    // timestamps) rather than re-deriving it. It was just made active, so a
    // missing row here is a real internal inconsistency.
    let account = store
        .get_account(account_id)
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok(ApiResponse::new(
        account_dto(account, sync_state.as_ref(), backup_state.as_ref()).await,
    ))
}

/// Log a Matrix account out, then return the resulting (now `deactivated`)
/// account. Stops syncing it, invalidates its device token upstream (best-effort
/// — an unreachable homeserver never fails the logout), and moves it to a
/// logged-out state, **retaining all of its data** (archive, search, media) so a
/// later login reactivates the same `account_id`. Idempotent: logging out an
/// already-logged-out account is a `200` no-op. An account mid-deletion
/// (`deleting`) is a `409`; an unknown id is a `404`.
///
/// Secret-bearing / destructive; gated by the bearer-token auth layer like every
/// `/v1/` route (M7b, ADR 0029).
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/logout",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
    ),
    responses(
        (status = 200, description = "The logged-out (deactivated) account", body = ApiResponse<AccountDto>),
        (status = 404, description = "No such account", body = crate::response::ErrorResponse),
        (status = 409, description = "The account is being deleted", body = crate::response::ErrorResponse),
    ),
    tag = "accounts",
)]
pub async fn logout(
    State(store): State<Store>,
    State(lifecycle): State<Arc<dyn AccountLifecycle>>,
    State(sync_state): State<Arc<dyn SyncStateProvider>>,
    State(backup_state): State<Arc<dyn BackupStateProvider>>,
    Path(account_id): Path<Uuid>,
) -> Result<ApiResponse<AccountDto>, ApiError> {
    lifecycle.logout(account_id).await?;
    // Read the row back so the response reflects the persisted (deactivated) state.
    // It was just transitioned, so a missing row here is a real inconsistency.
    let account = store
        .get_account(account_id)
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok(ApiResponse::new(
        account_dto(account, sync_state.as_ref(), backup_state.as_ref()).await,
    ))
}

/// Acquire E2EE keys for an **active** account from its Secure-Storage (4S)
/// recovery key, then return the account with its `verified` flag re-derived
/// and an honest megolm-backup action (ADR 0098). Cross-signing is imported
/// from 4S; if the homeserver has no megolm backup, recover auto-enables one
/// using this same key (it does not mint a new recovery key). Stored UTDs the
/// imported keys unlock are back-filled under a 30s cap. The recovery key is
/// used once and never persisted.
///
/// A `200` means **4S import succeeded**, not that history keys downloaded and
/// not that backup enabled. The flattened body keeps `account_id` / `verified`
/// at the top level for existing clients; `backup_action` and `redecrypt` are
/// additive siblings. `verified` is a derived observation of cross-signing.
///
/// The account must be `active`: a logged-out (`deactivated`) account is a `409`
/// (log in first), as is one mid-deletion (`deleting`). A wrong/rotated key, or an
/// account that never set up Secure Backup, is a `400` (a readable error, not a
/// silent permanent UTD). An unknown id is a `404`. Recover never 409s because
/// an existing homeserver backup needs joining.
///
/// Secret-bearing; gated by the bearer-token auth layer like every `/v1/` route
/// (M7b, ADR 0029).
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/recover",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
    ),
    request_body = RecoverRequest,
    responses(
        (status = 200, description = "4S import succeeded; flattened account plus `redecrypt` and `backup_action`", body = ApiResponse<RecoverResponseDto>),
        (status = 400, description = "The recovery key was wrong/rotated, or the account has no Secure Backup", body = crate::response::ErrorResponse),
        (status = 404, description = "No such account", body = crate::response::ErrorResponse),
        (status = 409, description = "The account is not active (logged out — log in first) or is being deleted", body = crate::response::ErrorResponse),
    ),
    tag = "accounts",
)]
pub async fn recover(
    State(store): State<Store>,
    State(lifecycle): State<Arc<dyn AccountLifecycle>>,
    State(sync_state): State<Arc<dyn SyncStateProvider>>,
    State(backup_state): State<Arc<dyn BackupStateProvider>>,
    Path(account_id): Path<Uuid>,
    Json(req): Json<RecoverRequest>,
) -> Result<ApiResponse<RecoverResponseDto>, ApiError> {
    let result = lifecycle.recover(account_id, &req.recovery_key).await?;
    // Read the row back so the response reflects the freshly-derived `verified`
    // state. The account was just operated on, so a missing row is a real
    // inconsistency.
    let account = store
        .get_account(account_id)
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok(ApiResponse::new(RecoverResponseDto {
        account: account_dto(account, sync_state.as_ref(), backup_state.as_ref()).await,
        redecrypt: RedecryptUtdsResponse::from(result.redecrypt),
        backup_action: result.backup_action.into(),
    }))
}

/// Originate megolm key backup, export `m.megolm_backup.v1` into existing 4S,
/// resume a crashed create, or kick an already-enabled upload (ADR 0098).
/// Never mints a new recovery key. Never deletes someone else's backup.
///
/// `recovery_key` is required for create, export-only, and crash-resume
/// replace. Omitting it is kick-upload only. Unverified device or a
/// homeserver backup this device is not connected to (and did not sign with
/// intent) is a `409`.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/backup/enable",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
    ),
    request_body = EnableBackupRequest,
    responses(
        (status = 200, description = "Flattened account plus `backup_action`", body = ApiResponse<EnableBackupResponseDto>),
        (status = 400, description = "recovery_key required, or the account has no 4S (Axon will not mint a new key)", body = crate::response::ErrorResponse),
        (status = 404, description = "No such account", body = crate::response::ErrorResponse),
        (status = 409, description = "Not active, unverified, or an existing HS backup this device is not connected to", body = crate::response::ErrorResponse),
    ),
    tag = "accounts",
)]
pub async fn enable_backup(
    State(store): State<Store>,
    State(lifecycle): State<Arc<dyn AccountLifecycle>>,
    State(sync_state): State<Arc<dyn SyncStateProvider>>,
    State(backup_state): State<Arc<dyn BackupStateProvider>>,
    Path(account_id): Path<Uuid>,
    Json(req): Json<EnableBackupRequest>,
) -> Result<ApiResponse<EnableBackupResponseDto>, ApiError> {
    let recovery_key = req
        .recovery_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty());
    let backup_action = lifecycle.enable_backup(account_id, recovery_key).await?;
    let account = store
        .get_account(account_id)
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok(ApiResponse::new(EnableBackupResponseDto {
        account: account_dto(account, sync_state.as_ref(), backup_state.as_ref()).await,
        backup_action: backup_action.into(),
    }))
}

/// Explicitly retry every pending UTD for an active account. The default startup
/// policy attempts each stored UTD once, then waits for fresh room-key arrivals;
/// this endpoint is the authenticated operator escape hatch for rows whose keys
/// are already in the crypto store or whose initial startup attempt predated key
/// acquisition.
///
/// The identity lock is held only for the active check and client lookup;
/// logout can proceed and cancels this retry. The HTTP cap is 10 minutes
/// (a reverse proxy may cut it earlier). A cap or cancel returns 200 with
/// partial counts and `timed_out=true`, not 504.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/utds/redecrypt",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
    ),
    responses(
        (status = 200, description = "Manual UTD re-decryption retry completed or timed out (10-minute cap or logout cancel)", body = ApiResponse<RedecryptUtdsResponse>),
        (status = 404, description = "No such account", body = crate::response::ErrorResponse),
        (status = 409, description = "The account is not active (logged out — log in first) or is being deleted", body = crate::response::ErrorResponse),
    ),
    tag = "accounts",
)]
pub async fn redecrypt_utds(
    State(lifecycle): State<Arc<dyn AccountLifecycle>>,
    Path(account_id): Path<Uuid>,
) -> Result<ApiResponse<RedecryptUtdsResponse>, ApiError> {
    let stats = lifecycle.redecrypt_utds(account_id).await?;
    Ok(ApiResponse::new(RedecryptUtdsResponse::from(stats)))
}

/// Permanently delete an account and every trace of it: stops syncing it,
/// invalidates its device token upstream, removes its on-disk SDK store, and drops
/// its row (cascading away its archived events, room state, and account data).
/// Returns `204 No Content` — the resource is gone, so there is nothing to return.
///
/// Unlike [`logout`] this is **not** reversible: re-adding the same Matrix account
/// later is a fresh login with a new `account_id`. Idempotent and crash-safe — a
/// delete of an account already mid-teardown resumes it, and an interrupted delete
/// is finished by the next boot's reconcile. An unknown id is a `404`. A `409` means
/// the account's sync task has not finished shutting down; retry shortly. This is
/// durable by construction — nothing auto-provisions accounts, so a deleted account
/// never comes back except through an explicit `login`/`import` call.
///
/// Destructive; gated by the bearer-token auth layer like every `/v1/` route
/// (M7b, ADR 0029).
#[utoipa::path(
    delete,
    path = "/v1/accounts/{account_id}",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
    ),
    responses(
        (status = 204, description = "The account was deleted (or was already gone after a resumed teardown)"),
        (status = 404, description = "No such account", body = crate::response::ErrorResponse),
        (status = 409, description = "The sync task is still shutting down; retry shortly", body = crate::response::ErrorResponse),
    ),
    tag = "accounts",
)]
pub async fn delete_account(
    State(lifecycle): State<Arc<dyn AccountLifecycle>>,
    Path(account_id): Path<Uuid>,
) -> Result<axum::http::StatusCode, ApiError> {
    lifecycle.delete(account_id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Read a single account by id, in whatever lifecycle state it is — unlike the
/// list, a direct by-id read is not filtered to `active` (so a client can poll
/// an account it knows and watch it transition). An unknown id is a 404.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
    ),
    responses(
        (status = 200, description = "The account", body = ApiResponse<AccountDto>),
        (status = 404, description = "No such account", body = crate::response::ErrorResponse),
    ),
    tag = "accounts",
)]
pub async fn get_account(
    State(store): State<Store>,
    State(sync_state): State<Arc<dyn SyncStateProvider>>,
    State(backup_state): State<Arc<dyn BackupStateProvider>>,
    Path(account_id): Path<Uuid>,
) -> Result<ApiResponse<AccountDto>, ApiError> {
    let account = store
        .get_account(account_id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("account {account_id} not found")))?;
    Ok(ApiResponse::new(
        account_dto(account, sync_state.as_ref(), backup_state.as_ref()).await,
    ))
}
