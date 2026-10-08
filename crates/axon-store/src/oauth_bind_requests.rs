//! Identity-bind handshake bookkeeping (M14, ADR 0054 "CLI bind command";
//! ADR 0109 for the management API's bind).
//!
//! A bind is started by `axon oauth bind --provider <p>` or by
//! `POST /v1/management/oauth/binds`. Either inserts a `pending` row and hands
//! out a URL containing `user_code`; the owner opens it in any browser, which
//! drives the same upstream OIDC redirect Path A uses (the row's own
//! `device_code` doubles as the `state` sent upstream); the starter polls
//! this row until `completed`/`expired`.

use chrono::{DateTime, Utc};
use sqlx_core::row::Row;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::{Store, StoreError};

/// One in-flight or finished bind handshake.
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

/// Serializes capped bind starts, so the pending count one of them reads is
/// still true when it inserts.
const BIND_START_LOCK_KEY: i64 = 0x4158_4f4e_4249_4e44;

// A macro, not a constant, so the insert below can be assembled at compile
// time from the one list of columns.
macro_rules! bind_request_columns {
    () => {
        "device_code, user_code, provider, status, created_at, \
         expires_at, oauth_identity_id, upstream_nonce"
    };
}

const BIND_REQUEST_COLUMNS: &str = bind_request_columns!();

const INSERT_BIND_REQUEST: &str = concat!(
    "INSERT INTO oauth_bind_requests (user_code, provider, expires_at, upstream_nonce) \
     VALUES ($1, $2, $3, $4) RETURNING ",
    bind_request_columns!()
);

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
    /// Cancel a pending bind so whoever is polling it stops and a fresh attempt can start.
    pub async fn cancel_bind_request(&self, id: Uuid) -> Result<bool, StoreError> {
        let result = sqlx_core::query::query("UPDATE oauth_bind_requests SET status = 'expired' WHERE device_code = $1 AND status = 'pending' AND expires_at > now()")
            .bind(id).execute(&self.pool).await?;
        Ok(result.rows_affected() == 1)
    }
    /// Create the `pending` row for a freshly-started bind handshake.
    /// Its immutable nonce is generated before the caller hands out the URL,
    /// so repeated GET/HEAD requests only read it and cannot consume the flow.
    ///
    /// Opportunistically sweeps expired rows first, same reasoning as
    /// [`create_authorization_request`](Self::create_authorization_request):
    /// this table has no other write path and starting a bind is
    /// owner-initiated, not a hot path.
    ///
    /// `user_code` is unique across the table, retained completed rows
    /// included; a taken code fails with an error for which
    /// [`StoreError::is_unique_violation`] is true.
    pub async fn create_bind_request(
        &self,
        provider: &str,
        user_code: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<BindRequest, StoreError> {
        self.delete_expired_bind_requests().await?;
        let request = Self::insert_bind_request(provider, user_code, expires_at)
            .fetch_one(&self.pool)
            .await?;
        Ok(request)
    }

    /// [`create_bind_request`](Self::create_bind_request), refused with
    /// `None` while `max_pending` binds are already waiting on a sign-in.
    ///
    /// Every pending row is a code the unauthenticated browser leg will
    /// accept, so the number outstanding multiplies a guesser's odds. The
    /// count and the insert run under one transaction-scoped advisory lock,
    /// so any number of simultaneous capped starts still leave at most
    /// `max_pending` rows between them. An uncapped
    /// [`create_bind_request`](Self::create_bind_request) does not take the
    /// lock and is counted here but never refused.
    pub async fn create_bind_request_unless_too_many(
        &self,
        provider: &str,
        user_code: &str,
        expires_at: DateTime<Utc>,
        max_pending: i64,
    ) -> Result<Option<BindRequest>, StoreError> {
        self.delete_expired_bind_requests().await?;
        let mut tx: Transaction<'_, Postgres> = self.pool.begin().await?;
        // Bound the wait: a start queues behind other starts only.
        sqlx_core::query::query("SET LOCAL lock_timeout = '5s'")
            .execute(&mut *tx)
            .await?;
        sqlx_core::query::query("SELECT pg_advisory_xact_lock($1)")
            .bind(BIND_START_LOCK_KEY)
            .execute(&mut *tx)
            .await?;
        let pending: i64 = sqlx_core::query::query(
            "SELECT count(*) AS pending FROM oauth_bind_requests \
              WHERE status = 'pending' AND expires_at > now()",
        )
        .fetch_one(&mut *tx)
        .await?
        .try_get("pending")?;
        if pending >= max_pending {
            tx.rollback().await?;
            return Ok(None);
        }
        let request = Self::insert_bind_request(provider, user_code, expires_at)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some(request))
    }

    fn insert_bind_request<'q>(
        provider: &'q str,
        user_code: &'q str,
        expires_at: DateTime<Utc>,
    ) -> sqlx_core::query_as::QueryAs<'q, Postgres, BindRequest, sqlx_postgres::PgArguments> {
        sqlx_core::query_as::query_as::<Postgres, BindRequest>(INSERT_BIND_REQUEST)
            .bind(user_code)
            .bind(provider)
            .bind(expires_at)
            .bind(axon_core::generate_opaque_secret())
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

    /// Delete the rows nobody can still need. A bind that did not complete
    /// goes as soon as its `expires_at` has lapsed. A completed one is kept a
    /// day past that: its status is the only record that the bind succeeded,
    /// and a client that polls late (a backgrounded phone, say) must find
    /// `completed`, not a missing row it would have to read as a failure.
    pub async fn delete_expired_bind_requests(&self) -> Result<u64, StoreError> {
        let result = sqlx_core::query::query(
            "DELETE FROM oauth_bind_requests \
              WHERE expires_at <= now() \
                AND (status <> 'completed' OR expires_at <= now() - interval '1 day')",
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
