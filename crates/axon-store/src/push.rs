//! Device push registrations (ADR 0113).
//!
//! A row is owned by a credential session. The device token is stored so an
//! upsert can find the row; it is not returned from these methods and must
//! not be logged. Revoke and unbind delete rows in the same transaction that
//! ends the session. Rotation does not.

use sqlx_core::row::Row;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::Postgres;
use uuid::Uuid;

use crate::{Store, StoreError};

/// A credential row that is still usable.
///
/// Not revoked, and not past `expires_at`. A null `expires_at` does not expire.
/// `oauth_refresh_tokens.expires_at` is `NOT NULL`, so that arm only changes
/// the meaning for access tokens. Reap, revoke, and registration all use this
/// text so the three checks cannot drift.
pub(crate) const LIVE_CREDENTIAL_SQL: &str =
    "revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now())";

/// What an upsert did. The device token is never in this value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushRegistrationOutcome {
    /// The row was inserted or re-homed.
    Stored(PushRegistration),
    /// The caller's access token was revoked or expired before the write.
    CredentialEnded,
    /// An expiring access token whose session has no live refresh token.
    RefreshRequired,
}

/// A registration as the HTTP API returns it. The device token is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRegistration {
    pub id: Uuid,
    pub provider: String,
    pub disclosure: String,
    pub environment: Option<String>,
}

/// What `PUT /v1/push/registrations` writes. `token` is the device token.
/// `Debug` redacts it.
pub struct NewPushRegistration<'a> {
    pub session_id: Uuid,
    pub token_id: Uuid,
    pub provider: &'a str,
    pub token: &'a str,
    pub environment: Option<&'a str>,
    pub disclosure: &'a str,
}

impl std::fmt::Debug for NewPushRegistration<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewPushRegistration")
            .field("session_id", &self.session_id)
            .field("token_id", &self.token_id)
            .field("provider", &self.provider)
            .field("token", &"[REDACTED]")
            .field("environment", &self.environment)
            .field("disclosure", &self.disclosure)
            .finish()
    }
}

impl Store {
    /// Insert or re-home the registration for `(provider, token)`.
    ///
    /// Holds the credential lock across the liveness check and the write, so a
    /// revoke that commits first cannot leave a row behind, and a revoke that
    /// waits deletes the row this call just wrote. The same device token keeps
    /// its id. The returned row does not include the token.
    ///
    /// An expiring access token is stored only when its session has a live
    /// refresh token. A non-expiring credential is stored while that token
    /// itself is still live.
    pub async fn upsert_push_registration(
        &self,
        registration: &NewPushRegistration<'_>,
    ) -> Result<PushRegistrationOutcome, StoreError> {
        let mut tx = self.pool.begin().await?;
        Self::lock_credentials(&mut tx).await?;
        // One statement. `gate` is the liveness of this call, and the insert
        // runs only when that row says the write is allowed. A rejected insert
        // still returns the booleans, so there is no second query.
        let sql = format!(
            "WITH gate AS ( \
                 SELECT \
                     EXISTS ( \
                         SELECT 1 FROM tokens \
                          WHERE id = $2 AND session_id = $1 AND {LIVE_CREDENTIAL_SQL} \
                     ) AS token_live, \
                     EXISTS ( \
                         SELECT 1 FROM tokens \
                          WHERE id = $2 AND session_id = $1 \
                            AND revoked_at IS NULL AND expires_at IS NULL \
                     ) AS non_expiring, \
                     EXISTS ( \
                         SELECT 1 FROM oauth_refresh_tokens \
                          WHERE session_id = $1 AND {LIVE_CREDENTIAL_SQL} \
                     ) AS refresh_live \
             ), written AS ( \
                 INSERT INTO push_registrations \
                     (session_id, token_id, provider, token, environment, disclosure) \
                 SELECT $1, $2, $3, $4, $5, $6 FROM gate \
                  WHERE token_live AND (non_expiring OR refresh_live) \
                 ON CONFLICT (provider, token) DO UPDATE \
                     SET session_id = EXCLUDED.session_id, \
                         token_id = EXCLUDED.token_id, \
                         environment = EXCLUDED.environment, \
                         disclosure = EXCLUDED.disclosure \
                 RETURNING id, provider, disclosure, environment \
             ) \
             SELECT written.id, written.provider, written.disclosure, written.environment, \
                    gate.token_live \
               FROM gate \
               LEFT JOIN written ON true"
        );
        let row = sqlx_core::query::query(&sql)
            .bind(registration.session_id)
            .bind(registration.token_id)
            .bind(registration.provider)
            .bind(registration.token)
            .bind(registration.environment)
            .bind(registration.disclosure)
            .fetch_one(&mut *tx)
            .await?;
        let id: Option<Uuid> = row.try_get("id")?;
        let token_live: bool = row.try_get("token_live")?;
        if let Some(id) = id {
            tx.commit().await?;
            return Ok(PushRegistrationOutcome::Stored(PushRegistration {
                id,
                provider: row.try_get("provider")?,
                disclosure: row.try_get("disclosure")?,
                environment: row.try_get("environment")?,
            }));
        }
        tx.rollback().await?;
        Ok(if token_live {
            PushRegistrationOutcome::RefreshRequired
        } else {
            PushRegistrationOutcome::CredentialEnded
        })
    }

    /// Remove the row when `session_id` owns it. An absent id, or another
    /// session's id, removes nothing. Either way this is success: the route
    /// answers `204` and does not say which one happened.
    pub async fn delete_push_registration(
        &self,
        id: Uuid,
        session_id: Uuid,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query("DELETE FROM push_registrations WHERE id = $1 AND session_id = $2")
            .bind(id)
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Whether this session still has a refresh token that can mint another
    /// access token. A revoked or expired row does not count, including one
    /// the sweep has not deleted yet. Another device's refresh token does not
    /// count: the match is `session_id`, not the identity or the client.
    pub async fn session_has_live_refresh_token(
        &self,
        session_id: Uuid,
    ) -> Result<bool, StoreError> {
        let live: bool = sqlx_core::query::query(&format!(
            "SELECT EXISTS ( \
                 SELECT 1 FROM oauth_refresh_tokens \
                  WHERE session_id = $1 AND {LIVE_CREDENTIAL_SQL} \
             ) AS live"
        ))
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?
        .try_get("live")?;
        Ok(live)
    }

    /// Delete registrations whose session has no live credential.
    ///
    /// A credential is live when `revoked_at` is null and it has not expired.
    /// A null `expires_at` does not expire. The check looks only at rows that
    /// remain, so a swept predecessor does not kill a session that still has
    /// a later live refresh token. Called at server boot. Returns how many
    /// registrations were removed.
    pub async fn reap_push_registrations(&self) -> Result<u64, StoreError> {
        let sql = format!(
            "DELETE FROM push_registrations AS registration \
              WHERE NOT EXISTS ( \
                    SELECT 1 FROM tokens \
                     WHERE tokens.session_id = registration.session_id \
                       AND {LIVE_CREDENTIAL_SQL} \
                  ) \
                AND NOT EXISTS ( \
                    SELECT 1 FROM oauth_refresh_tokens \
                     WHERE oauth_refresh_tokens.session_id = registration.session_id \
                       AND {LIVE_CREDENTIAL_SQL} \
                  )"
        );
        let result = sqlx_core::query::query(&sql).execute(&self.pool).await?;
        Ok(result.rows_affected())
    }
}

/// Drop every registration owned by one of `session_ids`. Empty is a no-op.
/// Callers run this in the transaction that ends those sessions.
pub(crate) async fn delete_push_registrations_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_ids: &[Uuid],
) -> Result<(), StoreError> {
    if session_ids.is_empty() {
        return Ok(());
    }
    sqlx_core::query::query("DELETE FROM push_registrations WHERE session_id = ANY($1)")
        .bind(session_ids)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
