//! Account rows: one per Matrix account this Axon process syncs.
//!
//! "One human per Axon process, N Matrix accounts inside" — every
//! account-scoped table references [`Account::account_id`]. Access and OAuth
//! refresh tokens are encrypted at rest with pgcrypto's `pgp_sym_encrypt`
//! (ADR 0008, ADR 0097); the symmetric key lives in config (`sync.store_key`)
//! and is passed in per call, never stored in the database.
//!
//! Queries use sqlx's runtime `query`/`query_as` API rather than the
//! compile-time macros (the macros require the `sqlx` umbrella we dropped — see
//! `migrations.rs` — and a database at build time). `FromRow` is implemented by
//! hand for the same reason.

use chrono::{DateTime, Utc};
use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::{Store, StoreError};

/// The Matrix authentication implementation that owns an account's session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountAuthKind {
    /// A Matrix Client-Server API access-token session.
    Matrix,
    /// A Matrix OAuth 2.0 session with refresh-token rotation.
    OAuth,
}

impl AccountAuthKind {
    /// The database representation (kept in sync with the migration `CHECK`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Matrix => "matrix",
            Self::OAuth => "oauth",
        }
    }

    fn from_db(s: &str) -> Result<Self, sqlx_core::Error> {
        match s {
            "matrix" => Ok(Self::Matrix),
            "oauth" => Ok(Self::OAuth),
            other => Err(sqlx_core::Error::ColumnDecode {
                index: "auth_kind".to_owned(),
                source: format!("unknown account auth kind {other:?}").into(),
            }),
        }
    }
}

/// A decrypted account session.
///
/// Deliberately does not implement [`Debug`] so accidental structured logging
/// cannot expose either token.
pub enum StoredAccountSession {
    /// A legacy Matrix access-token session.
    Matrix { access_token: String },
    /// An OAuth session, including the public client registration identifier.
    OAuth {
        access_token: String,
        refresh_token: String,
        client_id: String,
    },
}

/// A public OAuth client registration shared across Matrix accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixOAuthRegistration {
    /// Canonical authorization-server issuer URL.
    pub issuer_url: String,
    /// Homeserver URL through which this issuer was discovered.
    pub homeserver_url: String,
    /// Public OAuth client identifier (never a client secret).
    pub client_id: String,
}

/// An account's lifecycle state, kept orthogonal to verification (ADR 0022).
///
/// The sync engine and the mutations gateway connect and serve **only**
/// `Active` accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountState {
    /// The normal state: the account syncs and can send.
    Active,
    /// A reversible pause that retains all of axon's data, reached via logout or
    /// an internal token failure. Does not sync or send; a fresh login
    /// reactivates the same row.
    Deactivated,
    /// A transient teardown breadcrumb set while a delete is in flight. A
    /// boot-time reconcile drives any row left here to completion; it is never a
    /// resting state a client observes long-term.
    Deleting,
}

impl AccountState {
    /// The on-disk / on-the-wire string form (matches the migration's `CHECK`).
    pub fn as_str(self) -> &'static str {
        match self {
            AccountState::Active => "active",
            AccountState::Deactivated => "deactivated",
            AccountState::Deleting => "deleting",
        }
    }

    /// Parse the stored string back into a state, failing as a column-decode
    /// error so an unexpected value surfaces as a read error rather than a
    /// silent default.
    fn from_db(s: &str) -> Result<Self, sqlx_core::Error> {
        match s {
            "active" => Ok(AccountState::Active),
            "deactivated" => Ok(AccountState::Deactivated),
            "deleting" => Ok(AccountState::Deleting),
            other => Err(sqlx_core::Error::ColumnDecode {
                index: "state".to_owned(),
                source: format!("unknown account state {other:?}").into(),
            }),
        }
    }
}

impl std::fmt::Display for AccountState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A Matrix account row. Encrypted tokens are deliberately absent — they are
/// only ever read back through [`Store::account_session`], which decrypts in
/// SQL, so plaintext never lingers on this struct.
#[derive(Debug, Clone)]
pub struct Account {
    /// Stable primary key, referenced by every account-scoped table.
    pub account_id: Uuid,
    /// Full Matrix user ID, e.g. `@alice:example.org`.
    pub user_id: String,
    /// Homeserver base URL.
    pub homeserver_url: String,
    /// Device ID assigned at login (or supplied with a pre-provisioned token).
    pub device_id: Option<String>,
    /// Authentication implementation that owns the persisted session.
    pub auth_kind: AccountAuthKind,
    /// Lifecycle state (ADR 0022). The sync engine connects only [`AccountState::Active`] rows.
    pub state: AccountState,
    /// Whether axon's own device is currently cross-signed (orthogonal to
    /// [`state`](Self::state)). Derived from the SDK's cross-signing state and kept
    /// fresh by the sync engine's verification watcher (ADR 0026), written via
    /// [`Store::set_account_verified`]; `false` for a never-verified or
    /// not-yet-synced device.
    pub verified: bool,
    /// Reserved sync-position cursor; the SyncService manages its own position
    /// in its SQLite store, so this currently stays `NULL`.
    pub sync_token: Option<String>,
    /// Row creation time.
    pub created_at: DateTime<Utc>,
    /// Last update time.
    pub updated_at: DateTime<Utc>,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for Account {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        let state: String = row.try_get("state")?;
        let auth_kind: String = row.try_get("auth_kind")?;
        Ok(Account {
            account_id: row.try_get("account_id")?,
            user_id: row.try_get("user_id")?,
            homeserver_url: row.try_get("homeserver_url")?,
            device_id: row.try_get("device_id")?,
            auth_kind: AccountAuthKind::from_db(&auth_kind)?,
            state: AccountState::from_db(&state)?,
            verified: row.try_get("verified")?,
            sync_token: row.try_get("sync_token")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

/// Columns selected for an [`Account`] (no encrypted token).
const ACCOUNT_COLUMNS: &str = "account_id, user_id, homeserver_url, device_id, \
    auth_kind, state, verified, sync_token, created_at, updated_at";

impl Store {
    /// Insert the account for `(user_id, homeserver_url)`, or return the
    /// existing row if it is already provisioned. Idempotent, so it is safe to
    /// call on every boot.
    pub async fn upsert_account(
        &self,
        user_id: &str,
        homeserver_url: &str,
    ) -> Result<Account, StoreError> {
        let sql = format!(
            "INSERT INTO accounts (user_id, homeserver_url) VALUES ($1, $2) \
             ON CONFLICT (user_id, homeserver_url) \
             DO UPDATE SET updated_at = now() \
             RETURNING {ACCOUNT_COLUMNS}"
        );
        let account = sqlx_core::query_as::query_as::<Postgres, Account>(&sql)
            .bind(user_id)
            .bind(homeserver_url)
            .fetch_one(&self.pool)
            .await?;
        Ok(account)
    }

    /// Fetch a single account by id, or `None` if no such row exists. Used by
    /// the sync engine's client manager to cold-connect an account from just its
    /// id (e.g. when an API send arrives before sync has brought it online).
    pub async fn get_account(&self, account_id: Uuid) -> Result<Option<Account>, StoreError> {
        let sql = format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE account_id = $1");
        let account = sqlx_core::query_as::query_as::<Postgres, Account>(&sql)
            .bind(account_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(account)
    }

    /// The `active` accounts, oldest first — the safe default for the connect
    /// path. The sync engine iterates these to spawn one task per account, so a
    /// `deactivated` or `deleting` row is never brought online (ADR 0022).
    ///
    /// "List accounts" deliberately means "the accounts you act on", not "every
    /// row": surfacing `deactivated`/`deleting` rows (e.g. the read API showing a
    /// logged-out account, or the teardown reconcile finding `deleting` rows) is
    /// a separate, explicitly-named query added when such a caller lands — so the
    /// default can never silently include a stale row.
    pub async fn list_accounts(&self) -> Result<Vec<Account>, StoreError> {
        let sql = format!(
            "SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE state = 'active' ORDER BY created_at ASC"
        );
        let accounts = sqlx_core::query_as::query_as::<Postgres, Account>(&sql)
            .fetch_all(&self.pool)
            .await?;
        Ok(accounts)
    }

    /// The client-visible accounts — `active` **and** `deactivated`, oldest
    /// first — backing the lifecycle read API (`GET /v1/accounts`). It includes
    /// logged-out (`deactivated`) accounts so a client that has lost the
    /// `account_id` can still discover one to offer re-login, but excludes the
    /// transient `deleting` teardown state (a row mid-removal is not something to
    /// act on; a by-id [`get_account`](Self::get_account) read still surfaces it).
    ///
    /// Deliberately distinct from [`list_accounts`](Self::list_accounts), which is
    /// active-only for the connect/boot path — the two callers want different
    /// slices, so neither rides on a shared "all rows" accessor (ADR 0022).
    pub async fn list_client_visible_accounts(&self) -> Result<Vec<Account>, StoreError> {
        let sql = format!(
            "SELECT {ACCOUNT_COLUMNS} FROM accounts \
             WHERE state IN ('active', 'deactivated') ORDER BY created_at ASC"
        );
        let accounts = sqlx_core::query_as::query_as::<Postgres, Account>(&sql)
            .fetch_all(&self.pool)
            .await?;
        Ok(accounts)
    }

    /// The accounts mid-teardown (`deleting`), oldest first — the explicitly-named
    /// accessor the boot reconcile uses to re-find rows a crash left in flight
    /// (ADR 0022 / 0024). Distinct from the active-only [`list_accounts`](Self::list_accounts)
    /// and the client-facing [`list_client_visible_accounts`](Self::list_client_visible_accounts),
    /// so a stale `deleting` row can never leak onto the connect or read paths.
    pub async fn list_deleting_accounts(&self) -> Result<Vec<Account>, StoreError> {
        let sql = format!(
            "SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE state = 'deleting' ORDER BY created_at ASC"
        );
        let accounts = sqlx_core::query_as::query_as::<Postgres, Account>(&sql)
            .fetch_all(&self.pool)
            .await?;
        Ok(accounts)
    }

    /// Every account id, in **any** lifecycle state — the set the orphan-store-dir
    /// GC checks each `data_dir/<id>/` against (ADR 0024). Keyed off row existence,
    /// not state: a `deactivated` row is real and its dir must be kept, so GC prunes
    /// only dirs whose id matches *no* row here (the #24-safe distinction).
    pub async fn list_all_account_ids(&self) -> Result<Vec<Uuid>, StoreError> {
        let ids = sqlx_core::query::query("SELECT account_id FROM accounts")
            .try_map(|row: PgRow| row.try_get::<Uuid, _>("account_id"))
            .fetch_all(&self.pool)
            .await?;
        Ok(ids)
    }

    /// Look up an account by its Matrix user id before runtime login considers
    /// minting a new row. A Matrix id names one identity even when config and
    /// server-side discovery reach its homeserver through different base URLs.
    ///
    /// Returns the oldest active row first, then the oldest retained row in any
    /// other lifecycle state. The ordering keeps stores affected by the historical
    /// duplicate-account bug operable. Read-only — unlike
    /// [`upsert_account`](Self::upsert_account) it never inserts.
    pub async fn find_account_by_user_id(
        &self,
        user_id: &str,
    ) -> Result<Option<Account>, StoreError> {
        let sql = format!(
            "SELECT {ACCOUNT_COLUMNS} FROM accounts \
             WHERE user_id = $1 \
             ORDER BY (state = 'active') DESC, created_at ASC \
             LIMIT 1"
        );
        let account = sqlx_core::query_as::query_as::<Postgres, Account>(&sql)
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(account)
    }

    /// Move an account to a new lifecycle state (ADR 0022). The lifecycle verbs
    /// (login/logout/delete) own these transitions; clients never set `state`
    /// directly. The `updated_at` trigger maintains the timestamp.
    pub async fn set_account_state(
        &self,
        account_id: Uuid,
        state: AccountState,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query("UPDATE accounts SET state = $2 WHERE account_id = $1")
            .bind(account_id)
            .bind(state.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Set whether axon's own device is currently cross-signed (ADR 0026),
    /// orthogonal to lifecycle [`state`](Account::state). Unlike `state` this is a
    /// *derived* value, not a verb-driven transition: it is re-derived from the
    /// SDK's current cross-signing state (after `recover`/`verify` and whenever the
    /// SDK's verification state changes) and written here, so the column tracks
    /// reality rather than being written once. The `updated_at` trigger maintains
    /// the timestamp.
    pub async fn set_account_verified(
        &self,
        account_id: Uuid,
        verified: bool,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query("UPDATE accounts SET verified = $2 WHERE account_id = $1")
            .bind(account_id)
            .bind(verified)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Hard-delete the account row. The FK cascades (`ON DELETE CASCADE` on
    /// `events`, `room_state`, `account_data`, and the event crypto siblings)
    /// remove all of its Postgres-resident data in the same statement. Idempotent:
    /// deleting an already-gone row affects zero rows and is not an error.
    ///
    /// This is the **last** step of the account-delete teardown (in `axon-sync`) —
    /// the row is the durable key a boot reconcile uses to re-find external
    /// resources, so file-backed cleanup (SDK store dir, and later the media cache)
    /// runs first and the row is dropped only once it has (ADR 0024).
    ///
    /// The **search index** is handled differently: its purge obligation is
    /// appended to `search_outbox` in the *same statement* that drops the row, so
    /// it commits atomically and — crucially — *outlives* the row (the outbox has
    /// no FK to `accounts`). The indexer drains it whether or not search is
    /// currently enabled, so an account deleted while search is off, or a crash
    /// before the purge commits to Tantivy, still converges on the next enabled
    /// boot (ADR 0039). Idempotent: a re-run after the row is gone appends nothing.
    pub async fn delete_account_row(&self, account_id: Uuid) -> Result<(), StoreError> {
        sqlx_core::query::query(
            "WITH d AS ( \
               DELETE FROM accounts WHERE account_id = $1 RETURNING account_id \
             ) \
             INSERT INTO search_outbox (account_id, event_id) \
             SELECT account_id, '' FROM d",
        )
        .bind(account_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persist a legacy Matrix login session and clear any prior OAuth state.
    /// The plaintext token is bound as a parameter and never logged.
    pub async fn set_account_matrix_session(
        &self,
        account_id: Uuid,
        device_id: &str,
        access_token: &str,
        key: &str,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query(
            "UPDATE accounts \
             SET device_id = $2, \
                 auth_kind = 'matrix', \
                 access_token_encrypted = pgp_sym_encrypt($3, $4), \
                 oauth_refresh_token_encrypted = NULL, \
                 oauth_client_id = NULL \
             WHERE account_id = $1",
        )
        .bind(account_id)
        .bind(device_id)
        .bind(access_token)
        .bind(key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persist a complete Matrix OAuth session in one atomic database update.
    /// Both tokens are encrypted with `key`; no token is logged or stored in a
    /// struct that implements [`Debug`].
    pub async fn set_account_oauth_session(
        &self,
        account_id: Uuid,
        device_id: &str,
        access_token: &str,
        refresh_token: &str,
        client_id: &str,
        key: &str,
    ) -> Result<(), StoreError> {
        let result = sqlx_core::query::query(
            "UPDATE accounts \
             SET device_id = $2, \
                 auth_kind = 'oauth', \
                 access_token_encrypted = pgp_sym_encrypt($3, $6), \
                 oauth_refresh_token_encrypted = pgp_sym_encrypt($4, $6), \
                 oauth_client_id = $5 \
             WHERE account_id = $1",
        )
        .bind(account_id)
        .bind(device_id)
        .bind(access_token)
        .bind(refresh_token)
        .bind(client_id)
        .bind(key)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(StoreError::InvalidAccountSession(
                "account disappeared while storing OAuth session".to_owned(),
            ));
        }
        Ok(())
    }

    /// Atomically persist a rotated OAuth token pair and client ID.
    ///
    /// The authentication-kind predicate prevents a late refresh write from
    /// replacing a session that a concurrent lifecycle operation changed back
    /// to legacy Matrix authentication.
    pub async fn update_account_oauth_session(
        &self,
        account_id: Uuid,
        access_token: &str,
        refresh_token: &str,
        client_id: &str,
        key: &str,
    ) -> Result<(), StoreError> {
        let result = sqlx_core::query::query(
            "UPDATE accounts \
             SET access_token_encrypted = pgp_sym_encrypt($2, $5), \
                 oauth_refresh_token_encrypted = pgp_sym_encrypt($3, $5), \
                 oauth_client_id = $4 \
             WHERE account_id = $1 AND auth_kind = 'oauth'",
        )
        .bind(account_id)
        .bind(access_token)
        .bind(refresh_token)
        .bind(client_id)
        .bind(key)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(StoreError::OAuthSessionNotCurrent);
        }
        Ok(())
    }

    /// Decrypt and return the stored session, or `None` before first login.
    /// Decryption happens in SQL via `pgp_sym_decrypt`; ciphertext never leaves
    /// Postgres and inconsistent session shapes fail closed.
    pub async fn account_session(
        &self,
        account_id: Uuid,
        key: &str,
    ) -> Result<Option<StoredAccountSession>, StoreError> {
        let row = sqlx_core::query::query(
            "SELECT auth_kind, \
                    pgp_sym_decrypt(access_token_encrypted, $2) AS access_token, \
                    pgp_sym_decrypt(oauth_refresh_token_encrypted, $2) AS refresh_token, \
                    oauth_client_id \
             FROM accounts WHERE account_id = $1",
        )
        .bind(account_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else { return Ok(None) };
        let auth_kind = AccountAuthKind::from_db(row.try_get("auth_kind")?)?;
        let access_token: Option<String> = row.try_get("access_token")?;
        let refresh_token: Option<String> = row.try_get("refresh_token")?;
        let client_id: Option<String> = row.try_get("oauth_client_id")?;

        match (auth_kind, access_token, refresh_token, client_id) {
            (AccountAuthKind::Matrix, None, None, None) => Ok(None),
            (AccountAuthKind::Matrix, Some(access_token), None, None) => {
                Ok(Some(StoredAccountSession::Matrix { access_token }))
            }
            (AccountAuthKind::OAuth, Some(access_token), Some(refresh_token), Some(client_id)) => {
                Ok(Some(StoredAccountSession::OAuth {
                    access_token,
                    refresh_token,
                    client_id,
                }))
            }
            _ => Err(StoreError::InvalidAccountSession(
                "stored account session has an inconsistent shape".to_owned(),
            )),
        }
    }

    /// Find a persisted public OAuth client registration by canonical issuer.
    pub async fn matrix_oauth_registration(
        &self,
        issuer_url: &str,
    ) -> Result<Option<MatrixOAuthRegistration>, StoreError> {
        let row = sqlx_core::query::query(
            "SELECT issuer_url, homeserver_url, client_id \
             FROM matrix_oauth_registrations WHERE issuer_url = $1",
        )
        .bind(issuer_url)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| MatrixOAuthRegistration {
            issuer_url: row.get("issuer_url"),
            homeserver_url: row.get("homeserver_url"),
            client_id: row.get("client_id"),
        }))
    }

    /// Insert or replace a public OAuth client registration for an issuer.
    pub async fn upsert_matrix_oauth_registration(
        &self,
        registration: &MatrixOAuthRegistration,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query(
            "INSERT INTO matrix_oauth_registrations \
                 (issuer_url, homeserver_url, client_id) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (issuer_url) DO UPDATE \
             SET homeserver_url = EXCLUDED.homeserver_url, \
                 client_id = EXCLUDED.client_id",
        )
        .bind(&registration.issuer_url)
        .bind(&registration.homeserver_url)
        .bind(&registration.client_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update the reserved sync-position cursor. Currently unused (the
    /// SyncService owns its position) but kept for a future sync model that
    /// manages its own cursor.
    pub async fn update_sync_token(
        &self,
        account_id: Uuid,
        sync_token: &str,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query("UPDATE accounts SET sync_token = $2 WHERE account_id = $1")
            .bind(account_id)
            .bind(sync_token)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
