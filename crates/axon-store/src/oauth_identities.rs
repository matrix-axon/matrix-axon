//! Bound OAuth/OIDC identities (M14b, ADR 0054).
//!
//! What `axon oauth bind` (M14c) populates: proof that a specific upstream
//! provider subject (`sub`) belongs to this axon instance's one human owner.
//! Path A/B's token-minting handlers only ever *read* this table (matching a
//! verified id_token's `sub` against it); the write path (bind) is a
//! separate, deliberately out-of-band CLI verb, not something an
//! unauthenticated `/v1/oauth/*` request can trigger on its own.

use chrono::{DateTime, Utc};
use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::{Store, StoreError};

/// One bound identity: an upstream provider subject linked to this axon
/// instance's owner.
#[derive(Debug, Clone)]
pub struct OauthIdentity {
    pub id: Uuid,
    pub provider: String,
    pub subject: String,
    pub email: Option<String>,
    pub linked_at: DateTime<Utc>,
}

/// What an attempt to remove a bound identity did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityRemoval {
    /// The identity and every credential minted for it are gone.
    Removed,
    /// No identity has that id.
    NotFound,
    /// Refused: removing it would leave no surviving credential. Nothing was
    /// changed.
    LastCredential,
}

const OAUTH_IDENTITY_COLUMNS: &str = "id, provider, subject, email, linked_at";

impl sqlx_core::from_row::FromRow<'_, PgRow> for OauthIdentity {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(OauthIdentity {
            id: row.try_get("id")?,
            provider: row.try_get("provider")?,
            subject: row.try_get("subject")?,
            email: row.try_get("email")?,
            linked_at: row.try_get("linked_at")?,
        })
    }
}

impl Store {
    /// Bind a `(provider, subject)` identity, or update its stored `email` if
    /// already bound (an upstream email change should not require re-binding).
    /// The bind CLI (M14c) is the only caller; Path A/B's login handlers never
    /// call this — they only read via [`find_identity`](Self::find_identity).
    pub async fn bind_identity(
        &self,
        provider: &str,
        subject: &str,
        email: Option<&str>,
    ) -> Result<OauthIdentity, StoreError> {
        let sql = format!(
            "INSERT INTO oauth_identities (provider, subject, email) VALUES ($1, $2, $3) \
             ON CONFLICT (provider, subject) DO UPDATE SET email = EXCLUDED.email \
             RETURNING {OAUTH_IDENTITY_COLUMNS}"
        );
        let identity = sqlx_core::query_as::query_as::<Postgres, OauthIdentity>(&sql)
            .bind(provider)
            .bind(subject)
            .bind(email)
            .fetch_one(&self.pool)
            .await?;
        Ok(identity)
    }

    /// Look up a bound identity by its upstream `(provider, subject)` — the
    /// check every Path A/B login performs after verifying an id_token.
    pub async fn find_identity(
        &self,
        provider: &str,
        subject: &str,
    ) -> Result<Option<OauthIdentity>, StoreError> {
        let sql =
            format!("SELECT {OAUTH_IDENTITY_COLUMNS} FROM oauth_identities WHERE provider = $1 AND subject = $2");
        let identity = sqlx_core::query_as::query_as::<Postgres, OauthIdentity>(&sql)
            .bind(provider)
            .bind(subject)
            .fetch_optional(&self.pool)
            .await?;
        Ok(identity)
    }

    /// Look up a bound identity by its row id — used when finishing a
    /// refresh-token redemption, which only carries the identity's id (not
    /// its provider/subject).
    pub async fn find_identity_by_id(&self, id: Uuid) -> Result<Option<OauthIdentity>, StoreError> {
        let sql = format!("SELECT {OAUTH_IDENTITY_COLUMNS} FROM oauth_identities WHERE id = $1");
        let identity = sqlx_core::query_as::query_as::<Postgres, OauthIdentity>(&sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(identity)
    }

    /// All bound identities, newest first — the management read
    /// (`axon oauth identities list`, M14c).
    pub async fn list_identities(&self) -> Result<Vec<OauthIdentity>, StoreError> {
        let sql = format!(
            "SELECT {OAUTH_IDENTITY_COLUMNS} FROM oauth_identities ORDER BY linked_at DESC"
        );
        let identities = sqlx_core::query_as::query_as::<Postgres, OauthIdentity>(&sql)
            .fetch_all(&self.pool)
            .await?;
        Ok(identities)
    }

    /// Unbind an identity by id. Returns `true` if a row was deleted.
    /// Atomically revoke and detach access tokens (retaining their audit rows),
    /// revoke the non-expiring tokens its sessions minted through the
    /// management API (and any those minted in turn),
    /// remove refresh tokens and authorization requests, then delete the identity.
    /// No caller-side revocation is needed. A failure rolls back all changes.
    pub async fn delete_identity(&self, id: Uuid) -> Result<bool, StoreError> {
        Ok(self.remove_identity(id, None).await? == IdentityRemoval::Removed)
    }

    /// [`delete_identity`](Self::delete_identity), refused when it would leave
    /// the owner with no way to sign in (ADR 0109): no active non-expiring
    /// token, and no bound identity whose provider is in `usable_providers`.
    /// A refusal rolls everything back and removes nothing.
    ///
    /// The management API's unbind. The CLI keeps the unguarded form: an
    /// operator with a shell can always mint another token.
    pub async fn delete_identity_unless_last_credential(
        &self,
        id: Uuid,
        usable_providers: &[String],
    ) -> Result<IdentityRemoval, StoreError> {
        self.remove_identity(id, Some(usable_providers)).await
    }

    async fn remove_identity(
        &self,
        id: Uuid,
        guard: Option<&[String]>,
    ) -> Result<IdentityRemoval, StoreError> {
        let mut tx = self.pool.begin().await?;
        // Bound waits, including contention with an in-flight token rotation.
        sqlx_core::query::query("SET LOCAL lock_timeout = '5s'")
            .execute(&mut *tx)
            .await?;
        // Before any row lock, and before the guard counts: every
        // credential-removing write queues here, in one order.
        Self::lock_credentials(&mut tx).await?;
        // FOR UPDATE conflicts with the key-share locks taken by FK inserts:
        // credentials committed before this lock are cleaned up below; later
        // inserts cannot reference the deleted identity after we commit.
        let identity =
            sqlx_core::query::query("SELECT id FROM oauth_identities WHERE id = $1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if identity.is_none() {
            tx.rollback().await?;
            return Ok(IdentityRemoval::NotFound);
        }
        for sql in [
            // Tokens this identity's sessions minted through the management
            // API, and tokens those minted in turn (ADR 0109). They carry no
            // identity of their own, so without this a non-expiring token
            // minted from a session would outlive the unbind that was meant
            // to end everything that sign-in could do. Before the next
            // statement, which clears the link this one starts from.
            "WITH RECURSIVE minted AS ( \
                 SELECT child.id FROM tokens child \
                   JOIN tokens session ON child.created_by_token_id = session.id \
                  WHERE session.oauth_identity_id = $1 \
                 UNION \
                 SELECT child.id FROM tokens child \
                   JOIN minted ON child.created_by_token_id = minted.id \
             ) \
             UPDATE tokens SET revoked_at = now() \
              WHERE id IN (SELECT id FROM minted) AND revoked_at IS NULL",
            "UPDATE tokens SET revoked_at = COALESCE(revoked_at, now()), oauth_identity_id = NULL \
             WHERE oauth_identity_id = $1",
            // Delete the entire rotation chain in one statement so its
            // self-referencing replaced_by FK stays satisfied.
            "DELETE FROM oauth_refresh_tokens WHERE oauth_identity_id = $1",
            "DELETE FROM oauth_authorization_requests WHERE oauth_identity_id = $1",
        ] {
            sqlx_core::query::query(sql)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        // Completed bind requests already use ON DELETE SET NULL.
        sqlx_core::query::query("DELETE FROM oauth_identities WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        // Counted on the state this transaction would commit: the identity
        // and the tokens it backed are already gone from this view.
        if let Some(usable_providers) = guard {
            if !Self::surviving_credential_in_tx(&mut tx, usable_providers).await? {
                tx.rollback().await?;
                return Ok(IdentityRemoval::LastCredential);
            }
        }
        tx.commit().await?;
        Ok(IdentityRemoval::Removed)
    }
}
