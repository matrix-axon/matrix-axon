//! Durable, non-secret Matrix OAuth QR-login finalization breadcrumbs.
//!
//! The interactive flow itself is intentionally in memory.  This table records
//! only enough filesystem intent for boot reconciliation: either the staging
//! store is abandoned, or the encrypted OAuth session committed and adoption
//! must be completed before the account can become active.

use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::{Account, AccountState, Store, StoreError};

const ACCOUNT_COLUMNS: &str = "account_id, user_id, homeserver_url, device_id, \
    auth_kind, state, verified, sync_token, created_at, updated_at";

/// Filesystem finalization state persisted for crash reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixOAuthAcquireFinalization {
    /// The remote protocol has not committed an account session.  Boot removes
    /// the staging store and breadcrumb.
    Staging,
    /// The encrypted OAuth session is durable.  Boot adopts the staging store,
    /// activates the account, and removes the breadcrumb.
    SessionCommitted,
}

impl MatrixOAuthAcquireFinalization {
    fn from_db(value: &str) -> Result<Self, sqlx_core::Error> {
        match value {
            "staging" => Ok(Self::Staging),
            "session_committed" => Ok(Self::SessionCommitted),
            other => Err(sqlx_core::Error::ColumnDecode {
                index: "finalization_state".to_owned(),
                source: format!("unknown Matrix OAuth acquire finalization state {other:?}").into(),
            }),
        }
    }
}

/// One non-secret crash-recovery breadcrumb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixOAuthAcquireBreadcrumb {
    pub flow_id: Uuid,
    pub expected_user_id: String,
    pub presentation: String,
    pub staging_dir_name: String,
    pub finalization: MatrixOAuthAcquireFinalization,
    pub account_id: Option<Uuid>,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for MatrixOAuthAcquireBreadcrumb {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        let finalization: String = row.try_get("finalization_state")?;
        Ok(Self {
            flow_id: row.try_get("flow_id")?,
            expected_user_id: row.try_get("expected_user_id")?,
            presentation: row.try_get("presentation")?,
            staging_dir_name: row.try_get("staging_dir_name")?,
            finalization: MatrixOAuthAcquireFinalization::from_db(&finalization)?,
            account_id: row.try_get("account_id")?,
        })
    }
}

/// Result of atomically binding a completed SDK login to an Axon account.
#[derive(Debug)]
pub enum CommitMatrixOAuthAcquire {
    Committed(Account),
    ActiveConflict(Uuid),
    DeletingConflict(Uuid),
}

impl Store {
    /// Create the breadcrumb before any staging directory or remote work.
    /// Returns `false` when another flow already owns the expected Matrix ID.
    pub async fn create_matrix_oauth_acquire_breadcrumb(
        &self,
        flow_id: Uuid,
        expected_user_id: &str,
        presentation: &str,
        staging_dir_name: &str,
    ) -> Result<bool, StoreError> {
        let result = sqlx_core::query::query(
            "INSERT INTO matrix_oauth_acquire_flows \
                 (flow_id, expected_user_id, presentation, staging_dir_name) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (expected_user_id) DO NOTHING",
        )
        .bind(flow_id)
        .bind(expected_user_id)
        .bind(presentation)
        .bind(staging_dir_name)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// List every breadcrumb at boot, oldest first.
    pub async fn list_matrix_oauth_acquire_breadcrumbs(
        &self,
    ) -> Result<Vec<MatrixOAuthAcquireBreadcrumb>, StoreError> {
        let rows = sqlx_core::query_as::query_as::<Postgres, MatrixOAuthAcquireBreadcrumb>(
            "SELECT flow_id, expected_user_id, presentation, staging_dir_name, \
                    finalization_state, account_id \
             FROM matrix_oauth_acquire_flows ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Whether this identity has an unfinished QR-login finalization. Lifecycle
    /// login/import checks this under the same per-identity lock so neither can
    /// replace a session whose SDK-store adoption still needs reconciliation.
    pub async fn has_matrix_oauth_acquire_for_user(
        &self,
        expected_user_id: &str,
    ) -> Result<bool, StoreError> {
        let row = sqlx_core::query::query(
            "SELECT EXISTS(SELECT 1 FROM matrix_oauth_acquire_flows \
             WHERE expected_user_id = $1)",
        )
        .bind(expected_user_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get("exists")?)
    }

    /// Whether a flow is still before the encrypted-session commit point.
    /// Cleanup uses this to avoid touching a staging store that boot
    /// reconciliation must adopt after commit.
    pub async fn matrix_oauth_acquire_is_staging(&self, flow_id: Uuid) -> Result<bool, StoreError> {
        let row = sqlx_core::query::query(
            "SELECT EXISTS(SELECT 1 FROM matrix_oauth_acquire_flows \
             WHERE flow_id = $1 AND finalization_state = 'staging')",
        )
        .bind(flow_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get("exists")?)
    }

    /// Remove a pre-commit breadcrumb after cancellation, failure, or boot
    /// cleanup.  The state predicate prevents cleanup racing past the database
    /// commit point and discarding an account whose new session is durable.
    pub async fn abandon_matrix_oauth_acquire(&self, flow_id: Uuid) -> Result<bool, StoreError> {
        let result = sqlx_core::query::query(
            "DELETE FROM matrix_oauth_acquire_flows \
             WHERE flow_id = $1 AND finalization_state = 'staging'",
        )
        .bind(flow_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Persist the complete encrypted OAuth session and advance the breadcrumb
    /// in one Postgres transaction.  The account remains `deactivated` until
    /// the caller has adopted its SDK store on disk.
    #[allow(clippy::too_many_arguments)]
    pub async fn commit_matrix_oauth_acquire(
        &self,
        flow_id: Uuid,
        expected_user_id: &str,
        homeserver_url: &str,
        device_id: &str,
        access_token: &str,
        refresh_token: &str,
        client_id: &str,
        key: &str,
    ) -> Result<CommitMatrixOAuthAcquire, StoreError> {
        let mut tx = self.pool.begin().await?;
        let account_sql = format!(
            "SELECT {ACCOUNT_COLUMNS} FROM accounts \
             WHERE user_id = $1 \
             ORDER BY (state = 'active') DESC, created_at ASC \
             LIMIT 1 FOR UPDATE"
        );
        let existing = sqlx_core::query_as::query_as::<Postgres, Account>(&account_sql)
            .bind(expected_user_id)
            .fetch_optional(&mut *tx)
            .await?;

        let account = match existing {
            Some(account) if account.state == AccountState::Active => {
                tx.rollback().await?;
                return Ok(CommitMatrixOAuthAcquire::ActiveConflict(account.account_id));
            }
            Some(account) if account.state == AccountState::Deleting => {
                tx.rollback().await?;
                return Ok(CommitMatrixOAuthAcquire::DeletingConflict(
                    account.account_id,
                ));
            }
            Some(account) => account,
            None => {
                let sql = format!(
                    "INSERT INTO accounts (user_id, homeserver_url, state) \
                     VALUES ($1, $2, 'deactivated') RETURNING {ACCOUNT_COLUMNS}"
                );
                sqlx_core::query_as::query_as::<Postgres, Account>(&sql)
                    .bind(expected_user_id)
                    .bind(homeserver_url)
                    .fetch_one(&mut *tx)
                    .await?
            }
        };

        let updated = sqlx_core::query::query(
            "UPDATE accounts \
             SET homeserver_url = $8, device_id = $2, auth_kind = 'oauth', verified = false, \
                 access_token_encrypted = pgp_sym_encrypt($3, $7), \
                 oauth_refresh_token_encrypted = pgp_sym_encrypt($4, $7), \
                 oauth_client_id = $5 \
             WHERE account_id = $1 AND state = 'deactivated' AND user_id = $6",
        )
        .bind(account.account_id)
        .bind(device_id)
        .bind(access_token)
        .bind(refresh_token)
        .bind(client_id)
        .bind(expected_user_id)
        .bind(key)
        .bind(homeserver_url)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::InvalidAccountSession(
                "account changed while committing Matrix OAuth QR login".to_owned(),
            ));
        }

        let breadcrumb = sqlx_core::query::query(
            "UPDATE matrix_oauth_acquire_flows \
             SET finalization_state = 'session_committed', account_id = $2 \
             WHERE flow_id = $1 AND expected_user_id = $3 \
               AND finalization_state = 'staging' AND account_id IS NULL",
        )
        .bind(flow_id)
        .bind(account.account_id)
        .bind(expected_user_id)
        .execute(&mut *tx)
        .await?;
        if breadcrumb.rows_affected() != 1 {
            return Err(StoreError::InvalidAccountSession(
                "Matrix OAuth QR login breadcrumb changed before session commit".to_owned(),
            ));
        }
        tx.commit().await?;

        let account = self.get_account(account.account_id).await?.ok_or_else(|| {
            StoreError::InvalidAccountSession(
                "account disappeared after Matrix OAuth QR session commit".to_owned(),
            )
        })?;
        Ok(CommitMatrixOAuthAcquire::Committed(account))
    }

    /// Mark the adopted account active and consume its breadcrumb atomically.
    /// Returns `false` when the breadcrumb no longer names that account.
    pub async fn finalize_matrix_oauth_acquire(
        &self,
        flow_id: Uuid,
        account_id: Uuid,
    ) -> Result<bool, StoreError> {
        let mut tx = self.pool.begin().await?;
        let breadcrumb = sqlx_core::query::query(
            "DELETE FROM matrix_oauth_acquire_flows \
             WHERE flow_id = $1 AND account_id = $2 \
               AND finalization_state = 'session_committed'",
        )
        .bind(flow_id)
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
        if breadcrumb.rows_affected() != 1 {
            tx.rollback().await?;
            return Ok(false);
        }
        let active = sqlx_core::query::query(
            "UPDATE accounts SET state = 'active', verified = true \
             WHERE account_id = $1 AND state = 'deactivated'",
        )
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
        if active.rows_affected() != 1 {
            return Err(StoreError::InvalidAccountSession(
                "account changed before Matrix OAuth QR activation".to_owned(),
            ));
        }
        tx.commit().await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StoredAccountSession;

    async fn store() -> Store {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
        Store::connect(&url, 5).await.expect("connect + migrate")
    }

    fn user_id() -> String {
        format!("@qr{}:example.org", Uuid::new_v4().simple())
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn session_commit_and_activation_follow_the_breadcrumb() {
        let store = store().await;
        let flow_id = Uuid::new_v4();
        let user_id = user_id();
        let staging = flow_id.to_string();
        assert!(store
            .create_matrix_oauth_acquire_breadcrumb(flow_id, &user_id, "display", &staging)
            .await
            .unwrap());
        assert!(
            !store
                .create_matrix_oauth_acquire_breadcrumb(
                    Uuid::new_v4(),
                    &user_id,
                    "scan",
                    &Uuid::new_v4().to_string(),
                )
                .await
                .unwrap(),
            "only one flow may own an expected Matrix ID"
        );

        let account = match store
            .commit_matrix_oauth_acquire(
                flow_id,
                &user_id,
                "https://example.org/",
                "DEVICE",
                "access-secret",
                "refresh-secret",
                "public-client",
                "store-key",
            )
            .await
            .unwrap()
        {
            CommitMatrixOAuthAcquire::Committed(account) => account,
            other => panic!("unexpected commit outcome: {other:?}"),
        };
        assert_eq!(account.state, AccountState::Deactivated);
        assert!(!account.verified, "verification is hidden until adoption");
        let breadcrumbs = store.list_matrix_oauth_acquire_breadcrumbs().await.unwrap();
        let breadcrumb = breadcrumbs
            .iter()
            .find(|row| row.flow_id == flow_id)
            .unwrap();
        assert_eq!(
            breadcrumb.finalization,
            MatrixOAuthAcquireFinalization::SessionCommitted
        );
        assert_eq!(breadcrumb.account_id, Some(account.account_id));
        match store
            .account_session(account.account_id, "store-key")
            .await
            .unwrap()
            .unwrap()
        {
            StoredAccountSession::OAuth {
                access_token,
                refresh_token,
                client_id,
            } => {
                assert_eq!(access_token, "access-secret");
                assert_eq!(refresh_token, "refresh-secret");
                assert_eq!(client_id, "public-client");
            }
            StoredAccountSession::Matrix { .. } => panic!("expected OAuth session"),
        }

        assert!(store
            .finalize_matrix_oauth_acquire(flow_id, account.account_id)
            .await
            .unwrap());
        assert_eq!(
            store
                .get_account(account.account_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            AccountState::Active
        );
        assert!(
            store
                .get_account(account.account_id)
                .await
                .unwrap()
                .unwrap()
                .verified
        );
        assert!(
            !store
                .finalize_matrix_oauth_acquire(flow_id, account.account_id)
                .await
                .unwrap(),
            "finalization is single-use"
        );

        sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
            .bind(account.account_id)
            .execute(store.pool())
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn session_commit_refuses_an_identity_that_became_active() {
        let store = store().await;
        let user_id = user_id();
        let account = store
            .upsert_account(&user_id, "https://example.org/")
            .await
            .unwrap();
        let flow_id = Uuid::new_v4();
        assert!(
            store
                .create_matrix_oauth_acquire_breadcrumb(
                    flow_id,
                    &user_id,
                    "scan",
                    &flow_id.to_string(),
                )
                .await
                .unwrap()
        );

        assert!(matches!(
            store
                .commit_matrix_oauth_acquire(
                    flow_id,
                    &user_id,
                    "https://example.org/",
                    "DEVICE",
                    "access-secret",
                    "refresh-secret",
                    "public-client",
                    "store-key",
                )
                .await
                .unwrap(),
            CommitMatrixOAuthAcquire::ActiveConflict(id) if id == account.account_id
        ));
        assert!(store
            .matrix_oauth_acquire_is_staging(flow_id)
            .await
            .unwrap());
        assert!(store.abandon_matrix_oauth_acquire(flow_id).await.unwrap());

        sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
            .bind(account.account_id)
            .execute(store.pool())
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn session_commit_updates_a_retained_accounts_homeserver() {
        let store = store().await;
        let user_id = user_id();
        let account = store
            .upsert_account(&user_id, "https://old.example.org/")
            .await
            .unwrap();
        store
            .set_account_state(account.account_id, AccountState::Deactivated)
            .await
            .unwrap();
        let flow_id = Uuid::new_v4();
        assert!(
            store
                .create_matrix_oauth_acquire_breadcrumb(
                    flow_id,
                    &user_id,
                    "scan",
                    &flow_id.to_string(),
                )
                .await
                .unwrap()
        );

        let committed = match store
            .commit_matrix_oauth_acquire(
                flow_id,
                &user_id,
                "https://new.example.org/",
                "DEVICE",
                "access-secret",
                "refresh-secret",
                "public-client",
                "store-key",
            )
            .await
            .unwrap()
        {
            CommitMatrixOAuthAcquire::Committed(account) => account,
            other => panic!("unexpected commit outcome: {other:?}"),
        };
        assert_eq!(committed.account_id, account.account_id);
        assert_eq!(committed.homeserver_url, "https://new.example.org/");

        assert!(store
            .finalize_matrix_oauth_acquire(flow_id, account.account_id)
            .await
            .unwrap());
        sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
            .bind(account.account_id)
            .execute(store.pool())
            .await
            .unwrap();
    }
}
