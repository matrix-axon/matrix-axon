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
    /// remove refresh tokens and authorization requests, then delete the identity.
    /// No caller-side revocation is needed. A failure rolls back all changes.
    pub async fn delete_identity(&self, id: Uuid) -> Result<bool, StoreError> {
        let mut tx = self.pool.begin().await?;
        // Bound waits, including contention with an in-flight token rotation.
        sqlx_core::query::query("SET LOCAL lock_timeout = '5s'")
            .execute(&mut *tx)
            .await?;
        // FOR UPDATE conflicts with the key-share locks taken by FK inserts:
        // credentials committed before this lock are cleaned up below; later
        // inserts cannot reference the deleted identity after we commit.
        let identity =
            sqlx_core::query::query("SELECT id FROM oauth_identities WHERE id = $1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if identity.is_none() {
            return Ok(false);
        }
        for sql in [
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
        tx.commit().await?;
        Ok(true)
    }
}
