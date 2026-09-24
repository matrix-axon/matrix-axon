//! Client→axon bearer tokens: the local-API gate (M7b).
//!
//! A token authenticates a *client* to this Axon instance (distinct from the
//! Matrix access token in [`accounts`](crate::accounts), which authenticates
//! axon to a homeserver). Tokens are global to the instance, not account-scoped:
//! one human owns all their accounts.
//!
//! Only the **hash** of a token is stored — the raw secret is shown once at mint
//! and never recoverable. A token is high-entropy random, so a single SHA-256 is
//! the right primitive (the model used by e.g. GitHub personal access tokens),
//! rather than the recoverable `pgp_sym_encrypt` used for the access token or a
//! slow password KDF. Verification is one indexed equality lookup on the hash.
//!
//! The verification path lives behind [`Store::verify_token`] so a future OAuth
//! 2.0 issuer can replace the mint path (and this table) without changing the
//! on-the-wire `Authorization: Bearer` contract or any consumer code.
//!
//! As elsewhere in this crate the queries use sqlx's runtime API and `FromRow`
//! is hand-implemented (see `migrations.rs` for why the macros are unavailable).

use chrono::{DateTime, Utc};
use sqlx_core::row::Row;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::{Store, StoreError};

/// A token row as surfaced to the management CLI. The secret `hash` is
/// deliberately absent — listing tokens never exposes anything secret-bearing.
#[derive(Debug, Clone)]
pub struct Token {
    /// Stable id, used to revoke the token.
    pub id: Uuid,
    /// Human-supplied label, e.g. the device or purpose the token is for.
    pub label: String,
    /// When the token was minted.
    pub created_at: DateTime<Utc>,
    /// When the token was last accepted on a request, or `None` if never used.
    pub last_used_at: Option<DateTime<Utc>>,
    /// When the token was revoked, or `None` if still active.
    pub revoked_at: Option<DateTime<Utc>>,
    /// When this token stops verifying, or `None` for a CLI-minted token
    /// (which never expires). Set on OAuth-minted access tokens (M14b).
    pub expires_at: Option<DateTime<Utc>>,
    /// Which upstream OIDC provider (if any) backed the login that minted
    /// this token. `None` for a CLI-minted token.
    pub provider: Option<String>,
    /// The bound identity this token was minted for, if any.
    pub oauth_identity_id: Option<Uuid>,
    /// The registered OAuth client (`[[oauth.clients]]`) that redeemed the
    /// code/identity-token minting this row, if any.
    pub client_id: Option<String>,
}

impl Token {
    /// Whether the token has been revoked (and so fails verification).
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for Token {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(Token {
            id: row.try_get("id")?,
            label: row.try_get("label")?,
            created_at: row.try_get("created_at")?,
            last_used_at: row.try_get("last_used_at")?,
            revoked_at: row.try_get("revoked_at")?,
            expires_at: row.try_get("expires_at")?,
            provider: row.try_get("provider")?,
            oauth_identity_id: row.try_get("oauth_identity_id")?,
            client_id: row.try_get("client_id")?,
        })
    }
}

/// A freshly minted token: the row id plus the **raw** secret, returned exactly
/// once from [`Store::issue_token`] so the CLI can print it. The raw value is
/// never stored or recoverable afterward.
#[derive(Debug, Clone)]
pub struct IssuedToken {
    /// The stored token's id.
    pub id: Uuid,
    /// Its label.
    pub label: String,
    /// The raw bearer token. Show once; only its hash is persisted.
    pub token: String,
}

/// A freshly minted OAuth credential pair from the first-run web bootstrap.
/// The raw secrets are returned exactly once; only their hashes are persisted.
#[derive(Debug, Clone)]
pub struct IssuedOAuthTokenPair {
    pub access_token: String,
    pub refresh_token: String,
}

/// Columns selected for a [`Token`] (never the `hash`).
const TOKEN_COLUMNS: &str =
    "id, label, created_at, last_used_at, revoked_at, expires_at, provider, oauth_identity_id, client_id";

const BOOTSTRAP_LOCK_KEY: i64 = 0x4158_4f4e_424f_4f54;

impl Store {
    /// Whether the one-time web bootstrap may still create the first login
    /// credential. Historical credentials count: a revoked/expired token or an
    /// unbound OAuth identity still proves bootstrap was already used.
    pub async fn first_credential_bootstrap_available(&self) -> Result<bool, StoreError> {
        let available: bool = sqlx_core::query::query(
            "SELECT NOT EXISTS (SELECT 1 FROM accounts) \
                 AND NOT EXISTS (SELECT 1 FROM tokens) \
                 AND NOT EXISTS (SELECT 1 FROM oauth_identities) AS available",
        )
        .fetch_one(&self.pool)
        .await?
        .try_get("available")?;
        Ok(available)
    }

    /// Mint a new bearer token with the given label: generate a random secret,
    /// store its hash, and return the raw secret once (it is never recoverable
    /// afterward). This is the CLI bootstrap path (`axon token issue`).
    pub async fn issue_token(&self, label: &str) -> Result<IssuedToken, StoreError> {
        let token = generate_token();
        let hash = hash_token(&token);
        let row = sqlx_core::query::query(
            "INSERT INTO tokens (label, hash) VALUES ($1, $2) RETURNING id",
        )
        .bind(label)
        .bind(&hash)
        .fetch_one(&self.pool)
        .await?;
        Ok(IssuedToken {
            id: row.try_get("id")?,
            label: label.to_owned(),
            token,
        })
    }

    /// Mint the first bearer token through the temporary web bootstrap. Returns
    /// `None` if any account or prior credential already exists. The eligibility
    /// check runs under an advisory transaction lock so concurrent requests
    /// cannot both observe an empty instance and mint two first credentials.
    pub async fn issue_first_bootstrap_token(
        &self,
        label: &str,
    ) -> Result<Option<IssuedToken>, StoreError> {
        let mut tx: Transaction<'_, Postgres> = self.pool.begin().await?;
        self.lock_bootstrap(&mut tx).await?;
        if !Self::bootstrap_available_in_tx(&mut tx).await? {
            tx.rollback().await?;
            return Ok(None);
        }

        let token = generate_token();
        let hash = hash_token(&token);
        let row = sqlx_core::query::query(
            "INSERT INTO tokens (label, hash) VALUES ($1, $2) RETURNING id",
        )
        .bind(label)
        .bind(&hash)
        .fetch_one(&mut *tx)
        .await?;
        let id = row.try_get("id")?;
        tx.commit().await?;
        Ok(Some(IssuedToken {
            id,
            label: label.to_owned(),
            token,
        }))
    }

    /// Verify a presented bearer token. Hashes it, looks up an **unrevoked,
    /// unexpired** row, and (on a match) stamps `last_used_at`. Returns the
    /// token's id on success or `None` if the token is unknown, revoked, or
    /// expired. The match and the `last_used_at` touch happen in one
    /// statement so verification is a single round-trip on the hot path
    /// (every `/v1/` request goes through here). A CLI-minted token's
    /// `expires_at` is `NULL`, so `expires_at IS NULL` keeps it verifying
    /// forever exactly as before OAuth existed.
    pub async fn verify_token(&self, raw: &str) -> Result<Option<Uuid>, StoreError> {
        let hash = hash_token(raw);
        let row = sqlx_core::query::query(
            "UPDATE tokens SET last_used_at = now() \
             WHERE hash = $1 AND revoked_at IS NULL \
               AND (expires_at IS NULL OR expires_at > now()) \
             RETURNING id",
        )
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => Ok(Some(row.try_get("id")?)),
            None => Ok(None),
        }
    }

    /// Mint an OAuth-backed access token: like [`issue_token`](Self::issue_token),
    /// but carrying an expiry and the OAuth provenance columns. Used by the
    /// `oauth` module's token orchestration, never by the CLI.
    pub async fn issue_oauth_token(
        &self,
        label: &str,
        expires_at: DateTime<Utc>,
        provider: &str,
        oauth_identity_id: Uuid,
        client_id: &str,
    ) -> Result<IssuedToken, StoreError> {
        let token = generate_token();
        let hash = hash_token(&token);
        let row = sqlx_core::query::query(
            "INSERT INTO tokens (label, hash, expires_at, provider, oauth_identity_id, client_id) \
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
        )
        .bind(label)
        .bind(&hash)
        .bind(expires_at)
        .bind(provider)
        .bind(oauth_identity_id)
        .bind(client_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(IssuedToken {
            id: row.try_get("id")?,
            label: label.to_owned(),
            token,
        })
    }

    /// All tokens, newest first — the management read (`axon token list`).
    /// Includes revoked tokens so the audit trail is visible.
    pub async fn list_tokens(&self) -> Result<Vec<Token>, StoreError> {
        let sql = format!("SELECT {TOKEN_COLUMNS} FROM tokens ORDER BY created_at DESC");
        let tokens = sqlx_core::query_as::query_as::<Postgres, Token>(&sql)
            .fetch_all(&self.pool)
            .await?;
        Ok(tokens)
    }

    /// All still-active tokens with the given label — the lookup behind
    /// `axon token revoke --label`. Revoked tokens are excluded since a label
    /// only needs to be unambiguous among tokens that can still be revoked.
    pub async fn find_active_tokens_by_label(&self, label: &str) -> Result<Vec<Token>, StoreError> {
        let sql =
            format!("SELECT {TOKEN_COLUMNS} FROM tokens WHERE label = $1 AND revoked_at IS NULL");
        let tokens = sqlx_core::query_as::query_as::<Postgres, Token>(&sql)
            .bind(label)
            .fetch_all(&self.pool)
            .await?;
        Ok(tokens)
    }

    /// Revoke a token by id (stamp `revoked_at`). Returns `true` if a still-active
    /// token was revoked, `false` if no such id exists or it was already revoked —
    /// so the CLI can report the difference. Idempotent: the first revocation's
    /// timestamp is preserved (the `revoked_at IS NULL` guard makes a re-revoke a
    /// no-op).
    pub async fn revoke_token(&self, id: Uuid) -> Result<bool, StoreError> {
        let result = sqlx_core::query::query(
            "UPDATE tokens SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Revoke every still-active token minted for `oauth_identity_id`.
    /// Returns the number of tokens revoked without removing the identity.
    /// Unbinding uses [`delete_identity`](Self::delete_identity) instead,
    /// which invalidates all credentials and removes the identity atomically.
    pub async fn revoke_tokens_for_identity(
        &self,
        oauth_identity_id: Uuid,
    ) -> Result<u64, StoreError> {
        let result = sqlx_core::query::query(
            "UPDATE tokens SET revoked_at = now() \
              WHERE oauth_identity_id = $1 AND revoked_at IS NULL",
        )
        .bind(oauth_identity_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Bind the first OAuth identity and mint its access/refresh pair through
    /// the temporary web bootstrap. Returns `None` if bootstrap is no longer
    /// available. The identity and both tokens are written in one transaction.
    pub async fn issue_first_oauth_token_pair(
        &self,
        request: &crate::AuthorizationRequest,
        subject: &str,
        email: Option<&str>,
        access_expires_at: DateTime<Utc>,
        refresh_expires_at: DateTime<Utc>,
    ) -> Result<Option<IssuedOAuthTokenPair>, StoreError> {
        let mut tx: Transaction<'_, Postgres> = self.pool.begin().await?;
        self.lock_bootstrap(&mut tx).await?;
        if !Self::bootstrap_available_in_tx(&mut tx).await? {
            tx.rollback().await?;
            return Ok(None);
        }

        // The flow claim, identity, and token pair commit together. Cancellation,
        // expiry, or a concurrent callback cannot leave a bootstrap credential.
        let claimed = sqlx_core::query::query(
            "UPDATE oauth_authorization_requests SET status = 'redeemed' \
             WHERE id = $1 AND provider = $2 AND client_id = $3 AND redirect_uri = $4 \
               AND upstream_nonce = $5 AND status = 'pending' AND expires_at > clock_timestamp()",
        )
        .bind(request.id)
        .bind(&request.provider)
        .bind(&request.client_id)
        .bind(&request.redirect_uri)
        .bind(&request.upstream_nonce)
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() != 1 {
            tx.rollback().await?;
            return Ok(None);
        }
        let provider = request.provider.as_str();
        let client_id = request.client_id.as_str();

        let identity_id: Uuid = sqlx_core::query::query(
            "INSERT INTO oauth_identities (provider, subject, email) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(provider)
        .bind(subject)
        .bind(email)
        .fetch_one(&mut *tx)
        .await?
        .try_get("id")?;

        let access_token = generate_token();
        let access_hash = hash_token(&access_token);
        sqlx_core::query::query(
            "INSERT INTO tokens (label, hash, expires_at, provider, oauth_identity_id, client_id) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(format!("oauth:{provider}:{client_id}"))
        .bind(&access_hash)
        .bind(access_expires_at)
        .bind(provider)
        .bind(identity_id)
        .bind(client_id)
        .execute(&mut *tx)
        .await?;

        let refresh_token = generate_refresh_token();
        let refresh_hash = hash_token(&refresh_token);
        sqlx_core::query::query(
            "INSERT INTO oauth_refresh_tokens (hash, oauth_identity_id, client_id, expires_at) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&refresh_hash)
        .bind(identity_id)
        .bind(client_id)
        .bind(refresh_expires_at)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(Some(IssuedOAuthTokenPair {
            access_token,
            refresh_token,
        }))
    }

    async fn lock_bootstrap(&self, tx: &mut Transaction<'_, Postgres>) -> Result<(), StoreError> {
        sqlx_core::query::query("SELECT pg_advisory_xact_lock($1)")
            .bind(BOOTSTRAP_LOCK_KEY)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    async fn bootstrap_available_in_tx(
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<bool, StoreError> {
        let available: bool = sqlx_core::query::query(
            "SELECT NOT EXISTS (SELECT 1 FROM accounts) \
                 AND NOT EXISTS (SELECT 1 FROM tokens) \
                 AND NOT EXISTS (SELECT 1 FROM oauth_identities) AS available",
        )
        .fetch_one(&mut **tx)
        .await?
        .try_get("available")?;
        Ok(available)
    }
}

/// Generate a fresh bearer token: an `axon_` prefix (so it is recognizable and
/// greppable, like a GitHub `ghp_…` token) over 256 bits of CSPRNG entropy,
/// base64url-encoded without padding.
fn generate_token() -> String {
    format!("axon_{}", axon_core::generate_opaque_secret())
}

fn generate_refresh_token() -> String {
    format!("axon_rt_{}", axon_core::generate_opaque_secret())
}

/// Hash a raw token for storage / lookup: SHA-256, base64-encoded. A plain hash
/// (not a password KDF) is correct here because the input is high-entropy —
/// there is nothing to brute-force — and it must be cheap to run on every request.
fn hash_token(raw: &str) -> String {
    axon_core::hash_secret(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_prefixed_and_unique() {
        let a = generate_token();
        let b = generate_token();
        assert!(a.starts_with("axon_"));
        assert!(b.starts_with("axon_"));
        assert_ne!(a, b, "two mints must not collide");
    }

    #[test]
    fn hash_is_stable_and_distinguishes_inputs() {
        assert_eq!(hash_token("axon_abc"), hash_token("axon_abc"));
        assert_ne!(hash_token("axon_abc"), hash_token("axon_abd"));
        // The hash is not the raw token.
        assert_ne!(hash_token("axon_abc"), "axon_abc");
    }
}
