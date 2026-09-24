//! Single-use native challenges. All writes on redemption share one transaction.
use crate::{IssuedOAuthTokenPair, Store, StoreError};
use chrono::{DateTime, Utc};
use sqlx_core::{row::Row, transaction::Transaction};
use sqlx_postgres::Postgres;
use uuid::Uuid;

/// Shared by challenge storage and the public expires_in response.
pub const NATIVE_CHALLENGE_TTL_SECS: i32 = 300;

/// Only hashes of client-held capabilities are persisted.
pub struct NativeChallenge {
    pub hash: String,
    pub purpose: String,
    pub client_id: String,
    pub instance: String,
    pub nonce: String,
    pub authority_hash: Option<String>,
}

/// Verified upstream claims and local token lifetimes; never a raw upstream token.
pub struct IdentityRedemption<'a> {
    pub provider: &'a str,
    pub subject: &'a str,
    pub email: Option<&'a str>,
    pub replay_key: &'a str,
    pub client_id: &'a str,
    pub access_expires_at: DateTime<Utc>,
    pub refresh_expires_at: DateTime<Utc>,
}

impl Store {
    /// Bound storage even under distributed unauthenticated challenge requests.
    pub async fn create_native_challenge(&self, c: &NativeChallenge) -> Result<bool, StoreError> {
        let mut tx = self.pool.begin().await?;
        native_timeouts(&mut tx).await?;
        sqlx_core::query::query("SELECT pg_advisory_xact_lock(47085947794270)")
            .execute(&mut *tx)
            .await?;
        sqlx_core::query::query(
            "DELETE FROM oauth_native_challenges WHERE expires_at <= clock_timestamp()",
        )
        .execute(&mut *tx)
        .await?;
        let inserted = sqlx_core::query::query(
            "INSERT INTO oauth_native_challenges (hash, purpose, client_id, instance, nonce, authority_hash, expires_at)
             SELECT $1,$2,$3,$4,$5,$6,clock_timestamp() + make_interval(secs => $7)
             WHERE (SELECT count(*) FROM oauth_native_challenges
                    WHERE (purpose = 'login') = ($2 = 'login')) < $8")
            .bind(&c.hash).bind(&c.purpose).bind(&c.client_id).bind(&c.instance)
            .bind(&c.nonce).bind(&c.authority_hash).bind(f64::from(NATIVE_CHALLENGE_TTL_SECS))
            .bind(if c.purpose == "login" { 1024_i64 } else { 64_i64 })
            .execute(&mut *tx).await?.rows_affected() == 1;
        tx.commit().await?;
        Ok(inserted)
    }

    pub async fn native_challenge(
        &self,
        hash: &str,
    ) -> Result<Option<NativeChallenge>, StoreError> {
        let row = sqlx_core::query::query("SELECT * FROM oauth_native_challenges WHERE hash = $1 AND expires_at > clock_timestamp()")
            .bind(hash).fetch_optional(&self.pool).await?;
        row.map(|r| {
            Ok(NativeChallenge {
                hash: r.try_get("hash")?,
                purpose: r.try_get("purpose")?,
                client_id: r.try_get("client_id")?,
                instance: r.try_get("instance")?,
                nonce: r.try_get("nonce")?,
                authority_hash: r.try_get("authority_hash")?,
            })
        })
        .transpose()
    }

    /// Serializes consumption, owner authorization, binding, replay, and mint.
    /// A failed/aborted transaction burns nothing; a committed response lost in
    /// transit is recovered by starting a new Apple authorization.
    pub async fn redeem_identity_atomically(
        &self,
        r: &IdentityRedemption<'_>,
        challenge: Option<&NativeChallenge>,
    ) -> Result<Option<IssuedOAuthTokenPair>, StoreError> {
        let mut tx = self.pool.begin().await?;
        native_timeouts(&mut tx).await?;
        let purpose = challenge.map_or("login", |c| c.purpose.as_str());
        if purpose == "bootstrap" {
            self.lock_bootstrap(&mut tx).await?;
            if !Self::bootstrap_available_in_tx(&mut tx).await? {
                return Ok(None);
            }
        }
        if let Some(c) = challenge {
            // Recheck every immutable field read before upstream verification.
            let claimed = sqlx_core::query::query(
                "DELETE FROM oauth_native_challenges WHERE hash=$1 AND purpose=$2
                 AND client_id=$3 AND instance=$4 AND nonce=$5
                 AND authority_hash IS NOT DISTINCT FROM $6 AND expires_at > clock_timestamp()",
            )
            .bind(&c.hash)
            .bind(&c.purpose)
            .bind(r.client_id)
            .bind(&c.instance)
            .bind(&c.nonce)
            .bind(&c.authority_hash)
            .execute(&mut *tx)
            .await?;
            if claimed.rows_affected() != 1 {
                return Ok(None);
            }
            if purpose == "bind" {
                // FOR SHARE conflicts with token revocation. Authorization must
                // still be valid at redemption, not merely at challenge creation.
                let owner = sqlx_core::query::query(
                    "SELECT id FROM tokens WHERE hash=$1 AND revoked_at IS NULL
                     AND (expires_at IS NULL OR expires_at > clock_timestamp()) FOR SHARE",
                )
                .bind(&c.authority_hash)
                .fetch_optional(&mut *tx)
                .await?;
                if owner.is_none() {
                    return Ok(None);
                }
            }
        }
        if purpose != "login" {
            sqlx_core::query::query(
                "INSERT INTO oauth_identities (provider, subject, email) VALUES ($1,$2,$3)
                 ON CONFLICT (provider, subject) DO NOTHING",
            )
            .bind(r.provider)
            .bind(r.subject)
            .bind(r.email)
            .execute(&mut *tx)
            .await?;
        }
        // The row lock prevents unbind from racing a successful credential mint.
        let identity = sqlx_core::query::query(
            "SELECT id FROM oauth_identities WHERE provider=$1 AND subject=$2 FOR KEY SHARE",
        )
        .bind(r.provider)
        .bind(r.subject)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(identity) = identity else {
            return Ok(None);
        };
        let identity_id: Uuid = identity.try_get("id")?;
        let fresh = sqlx_core::query::query(
            "INSERT INTO oauth_consumed_identity_tokens (provider,replay_key) VALUES ($1,$2)
             ON CONFLICT (provider,replay_key) DO NOTHING",
        )
        .bind(r.provider)
        .bind(r.replay_key)
        .execute(&mut *tx)
        .await?;
        if fresh.rows_affected() != 1 {
            return Ok(None);
        }
        let pair = Self::mint_oauth_pair_in_tx(
            &mut tx,
            r.provider,
            identity_id,
            r.client_id,
            r.access_expires_at,
            r.refresh_expires_at,
        )
        .await?;
        tx.commit().await?;
        Ok(Some(pair))
    }
}

async fn native_timeouts(tx: &mut Transaction<'_, Postgres>) -> Result<(), StoreError> {
    sqlx_core::query::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut **tx)
        .await?;
    sqlx_core::query::query("SET LOCAL statement_timeout = '10s'")
        .execute(&mut **tx)
        .await?;
    Ok(())
}
