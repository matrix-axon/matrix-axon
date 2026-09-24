//! `axon oauth bind` device-code handshake bookkeeping (M14, ADR 0054 "CLI
//! bind command").
//!
//! The CLI (`axon oauth bind --provider <p>`, talking straight to [`Store`],
//! same pattern as `axon token issue`) inserts a `pending` row and prints a
//! URL containing `user_code`; an admin opens it in any browser, which
//! drives the same upstream OIDC redirect Path A uses (the row's own
//! `device_code` doubles as the `state` sent upstream); the CLI polls this
//! row directly via `Store` until `completed`/`expired`.

use chrono::{DateTime, Utc};
use sqlx_core::row::Row;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::{Store, StoreError};

/// One in-flight or finished `axon oauth bind` handshake.
#[derive(Debug, Clone)]
pub struct BindRequest {
    pub device_code: Uuid,
    pub user_code: String,
    pub provider: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub oauth_identity_id: Option<Uuid>,
    pub upstream_nonce: Option<String>,
}

const BIND_REQUEST_COLUMNS: &str = "device_code, user_code, provider, status, created_at, \
     expires_at, oauth_identity_id, upstream_nonce";

impl sqlx_core::from_row::FromRow<'_, PgRow> for BindRequest {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(BindRequest {
            device_code: row.try_get("device_code")?,
            user_code: row.try_get("user_code")?,
            provider: row.try_get("provider")?,
            status: row.try_get("status")?,
            created_at: row.try_get("created_at")?,
            expires_at: row.try_get("expires_at")?,
            oauth_identity_id: row.try_get("oauth_identity_id")?,
            upstream_nonce: row.try_get("upstream_nonce")?,
        })
    }
}

impl Store {
    /// Cancel a pending bind so the polling CLI exits and a fresh attempt can start.
    pub async fn cancel_bind_request(&self, id: Uuid) -> Result<bool, StoreError> {
        let result = sqlx_core::query::query("UPDATE oauth_bind_requests SET status = 'expired' WHERE device_code = $1 AND status = 'pending' AND expires_at > now()")
            .bind(id).execute(&self.pool).await?;
        Ok(result.rows_affected() == 1)
    }
    /// Create the `pending` row for a freshly-started bind handshake.
    /// Its immutable nonce is generated before the CLI prints the URL, so
    /// repeated GET/HEAD requests only read it and cannot consume the flow.
    ///
    /// Opportunistically sweeps expired rows first, same reasoning as
    /// [`create_authorization_request`](Self::create_authorization_request):
    /// this table has no other write path and `axon oauth bind` is
    /// operator-initiated, not a hot path.
    pub async fn create_bind_request(
        &self,
        provider: &str,
        user_code: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<BindRequest, StoreError> {
        self.delete_expired_bind_requests().await?;
        let sql = format!(
            "INSERT INTO oauth_bind_requests (user_code, provider, expires_at, upstream_nonce) \
             VALUES ($1, $2, $3, $4) RETURNING {BIND_REQUEST_COLUMNS}"
        );
        let request = sqlx_core::query_as::query_as::<Postgres, BindRequest>(&sql)
            .bind(user_code)
            .bind(provider)
            .bind(expires_at)
            .bind(axon_core::generate_opaque_secret())
            .fetch_one(&self.pool)
            .await?;
        Ok(request)
    }

    /// Look up a still-redeemable bind request by the code the admin typed
    /// into the browser — what `GET /v1/oauth/bind?user_code=...` starts
    /// the upstream redirect from.
    pub async fn find_bind_request_by_user_code(
        &self,
        user_code: &str,
    ) -> Result<Option<BindRequest>, StoreError> {
        let sql = format!(
            "SELECT {BIND_REQUEST_COLUMNS} FROM oauth_bind_requests \
              WHERE user_code = $1 AND status = 'pending' AND expires_at > now()"
        );
        let request = sqlx_core::query_as::query_as::<Postgres, BindRequest>(&sql)
            .bind(user_code)
            .fetch_optional(&self.pool)
            .await?;
        Ok(request)
    }

    /// Look up a bind request by its `device_code` — unfiltered by
    /// `status`/`expires_at`, unlike the other lookups here, because both of
    /// this method's callers need to observe terminal states too: the
    /// callback route (to disambiguate a bind-flow `state` from a Path A
    /// `state`) and the CLI's poll loop (to notice `completed`/`expired`).
    pub async fn find_bind_request(
        &self,
        device_code: Uuid,
    ) -> Result<Option<BindRequest>, StoreError> {
        let sql = format!(
            "SELECT {BIND_REQUEST_COLUMNS} FROM oauth_bind_requests WHERE device_code = $1"
        );
        let request = sqlx_core::query_as::query_as::<Postgres, BindRequest>(&sql)
            .bind(device_code)
            .fetch_optional(&self.pool)
            .await?;
        Ok(request)
    }

    /// Terminal `pending` -> `completed` transition, **and** the identity
    /// bind itself, in one transaction: claims the row first (the atomic
    /// conditional `UPDATE`, mirroring
    /// [`complete_authorization`](Self::complete_authorization)) and only
    /// then writes `oauth_identities` — never the other way around. Doing
    /// the identity UPSERT unconditionally *before* checking the claim, as
    /// an earlier version of this code did, let an expired-but-not-yet-swept
    /// request still permanently bind an identity even though the HTTP
    /// response ended up `409`: the write and the check were separate,
    /// unguarded statements with no rollback between them. Returns the bound
    /// identity's id, or `None` if the row wasn't `pending`/unexpired (stale,
    /// replayed, or lost a race against another completion) — in which case
    /// nothing is written at all.
    pub async fn complete_bind_request(
        &self,
        device_code: Uuid,
        provider: &str,
        subject: &str,
        email: Option<&str>,
    ) -> Result<Option<Uuid>, StoreError> {
        let mut tx: Transaction<'_, Postgres> = self.pool.begin().await?;

        let claimed = sqlx_core::query::query(
            "UPDATE oauth_bind_requests SET status = 'completed' \
              WHERE device_code = $1 AND provider = $2 \
                AND status = 'pending' AND expires_at > now()",
        )
        .bind(device_code)
        .bind(provider)
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(None);
        }

        let identity_id: Uuid = sqlx_core::query::query(
            "INSERT INTO oauth_identities (provider, subject, email) VALUES ($1, $2, $3) \
             ON CONFLICT (provider, subject) DO UPDATE SET email = EXCLUDED.email \
             RETURNING id",
        )
        .bind(provider)
        .bind(subject)
        .bind(email)
        .fetch_one(&mut *tx)
        .await?
        .try_get("id")?;

        sqlx_core::query::query(
            "UPDATE oauth_bind_requests SET oauth_identity_id = $2 WHERE device_code = $1",
        )
        .bind(device_code)
        .bind(identity_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(Some(identity_id))
    }

    /// Delete every row whose `expires_at` has lapsed, regardless of
    /// `status` — mirrors
    /// [`delete_expired_authorization_requests`](Self::delete_expired_authorization_requests).
    pub async fn delete_expired_bind_requests(&self) -> Result<u64, StoreError> {
        let result =
            sqlx_core::query::query("DELETE FROM oauth_bind_requests WHERE expires_at <= now()")
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected())
    }
}
