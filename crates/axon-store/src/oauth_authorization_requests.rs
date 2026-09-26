//! Path A (web-redirect) server-side bookkeeping (M14b, ADR 0054).
//!
//! Path A is a double-redirect (client -> axon -> upstream provider -> axon),
//! so axon cannot rely on any client-side session surviving the hop through
//! the upstream IdP's domain. This table is that state, and every transition
//! is a single conditional `UPDATE ... WHERE status = '<from>'` — the
//! concurrency control, never read-then-write.

use chrono::{DateTime, Utc};
use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::{Store, StoreError};

/// One Path A flow's server-side state.
#[derive(Debug, Clone)]
pub struct AuthorizationRequest {
    pub id: Uuid,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub client_state: Option<String>,
    pub provider: String,
    pub upstream_state: String,
    pub upstream_nonce: String,
    pub expires_at: DateTime<Utc>,
    pub status: String,
    pub oauth_identity_id: Option<Uuid>,
}

const AUTH_REQUEST_COLUMNS: &str = "id, client_id, redirect_uri, code_challenge, \
     code_challenge_method, client_state, provider, upstream_state, upstream_nonce, \
     expires_at, status, oauth_identity_id";

impl sqlx_core::from_row::FromRow<'_, PgRow> for AuthorizationRequest {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(AuthorizationRequest {
            id: row.try_get("id")?,
            client_id: row.try_get("client_id")?,
            redirect_uri: row.try_get("redirect_uri")?,
            code_challenge: row.try_get("code_challenge")?,
            code_challenge_method: row.try_get("code_challenge_method")?,
            client_state: row.try_get("client_state")?,
            provider: row.try_get("provider")?,
            upstream_state: row.try_get("upstream_state")?,
            upstream_nonce: row.try_get("upstream_nonce")?,
            expires_at: row.try_get("expires_at")?,
            status: row.try_get("status")?,
            oauth_identity_id: row.try_get("oauth_identity_id")?,
        })
    }
}

/// Fields needed to start a Path A flow — everything `GET /v1/oauth/authorize`
/// knows before it ever redirects to the upstream provider.
pub struct NewAuthorizationRequest<'a> {
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub code_challenge: &'a str,
    pub code_challenge_method: &'a str,
    pub client_state: Option<&'a str>,
    pub provider: &'a str,
    pub upstream_state: &'a str,
    pub upstream_nonce: &'a str,
    pub expires_at: DateTime<Utc>,
}

impl Store {
    /// Cancel a pending upstream flow without racing successful completion.
    pub async fn cancel_authorization(&self, id: Uuid) -> Result<bool, StoreError> {
        let result = sqlx_core::query::query("UPDATE oauth_authorization_requests SET status = 'expired' WHERE id = $1 AND status = 'pending' AND expires_at > now()")
            .bind(id).execute(&self.pool).await?;
        Ok(result.rows_affected() == 1)
    }

    /// Create the `pending` row for a freshly-started Path A flow. The
    /// returned id is both this row's primary key and the value baked into
    /// the redirect axon sends the browser to next.
    ///
    /// Opportunistically sweeps expired rows first: this table has no other
    /// write path, `/v1/oauth/authorize` is low-volume (a login start, not a
    /// per-request hot path), and the 10-minute flow TTL means the sweep
    /// never has much to do, so a dedicated background job would be pure
    /// overhead for what it buys.
    pub async fn create_authorization_request(
        &self,
        new: &NewAuthorizationRequest<'_>,
    ) -> Result<Uuid, StoreError> {
        self.delete_expired_authorization_requests().await?;
        let row = sqlx_core::query::query(
            "INSERT INTO oauth_authorization_requests \
                 (client_id, redirect_uri, code_challenge, code_challenge_method, \
                  client_state, provider, upstream_state, upstream_nonce, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id",
        )
        .bind(new.client_id)
        .bind(new.redirect_uri)
        .bind(new.code_challenge)
        .bind(new.code_challenge_method)
        .bind(new.client_state)
        .bind(new.provider)
        .bind(new.upstream_state)
        .bind(new.upstream_nonce)
        .bind(new.expires_at)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get("id")?)
    }

    /// Look up a pending flow by the `state` axon sent to the upstream
    /// provider and the provider named in the callback URL — what
    /// `GET /v1/oauth/{provider}/callback` uses to find which flow just
    /// returned. Both must match: `upstream_state` alone is not
    /// provider-scoped (it's provider-agnostic random bytes), so a callback
    /// hitting the wrong provider's path with a leaked `state` value must
    /// not resolve to another provider's pending flow.
    pub async fn find_authorization_request_by_upstream_state(
        &self,
        provider: &str,
        upstream_state: &str,
    ) -> Result<Option<AuthorizationRequest>, StoreError> {
        let sql = format!(
            "SELECT {AUTH_REQUEST_COLUMNS} FROM oauth_authorization_requests \
              WHERE upstream_state = $1 AND provider = $2 \
                AND status = 'pending' AND expires_at > now()"
        );
        let request = sqlx_core::query_as::query_as::<Postgres, AuthorizationRequest>(&sql)
            .bind(upstream_state)
            .bind(provider)
            .fetch_optional(&self.pool)
            .await?;
        Ok(request)
    }

    /// Transition `pending` -> `code_issued` in one atomic statement: the
    /// provider's callback landed, the id_token verified against a bound
    /// identity, and axon minted its own authorization code — all recorded
    /// together so a crash or error between "identity verified" and "code
    /// minted" can never leave the row stranded in an intermediate state
    /// with no path back out except waiting for `expires_at` to lapse.
    /// Returns `false` (no-op) if the row wasn't in `pending` or has
    /// expired — callers treat that as "stale or replayed callback".
    pub async fn complete_authorization(
        &self,
        id: Uuid,
        oauth_identity_id: Uuid,
        axon_code_hash: &str,
    ) -> Result<bool, StoreError> {
        let result = sqlx_core::query::query(
            "UPDATE oauth_authorization_requests \
                SET status = 'code_issued', oauth_identity_id = $2, axon_code_hash = $3 \
              WHERE id = $1 AND status = 'pending' AND expires_at > now()",
        )
        .bind(id)
        .bind(oauth_identity_id)
        .bind(axon_code_hash)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Redeem an axon-minted authorization code: the terminal
    /// `code_issued` -> `redeemed` transition, consuming it exactly once.
    /// Returns the full row (so the caller can verify PKCE's `code_verifier`
    /// against `code_challenge` and confirm `redirect_uri`/`client_id` match
    /// the token request) or `None` if the hash doesn't match a currently
    /// redeemable row (unknown, wrong status, or expired — all folded
    /// together since none should distinguish themselves to the client).
    pub async fn redeem_authorization_code(
        &self,
        axon_code_hash: &str,
    ) -> Result<Option<AuthorizationRequest>, StoreError> {
        let sql = format!(
            "UPDATE oauth_authorization_requests SET status = 'redeemed' \
              WHERE axon_code_hash = $1 AND status = 'code_issued' AND expires_at > now() \
              RETURNING {AUTH_REQUEST_COLUMNS}"
        );
        let request = sqlx_core::query_as::query_as::<Postgres, AuthorizationRequest>(&sql)
            .bind(axon_code_hash)
            .fetch_optional(&self.pool)
            .await?;
        Ok(request)
    }

    /// Delete every row whose `expires_at` has already lapsed, regardless of
    /// `status` — a `pending` row that never got a callback, a `code_issued`
    /// row nobody redeemed, or an already-`redeemed`/`expired` row, all stop
    /// mattering the moment they're unreachable by any live lookup. Keeps
    /// [`create_authorization_request`](Self::create_authorization_request)'s
    /// opportunistic sweep from growing this table without bound.
    pub async fn delete_expired_authorization_requests(&self) -> Result<u64, StoreError> {
        let result = sqlx_core::query::query(
            "DELETE FROM oauth_authorization_requests WHERE expires_at <= now()",
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
