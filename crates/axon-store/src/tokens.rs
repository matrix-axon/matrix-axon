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
    /// The token that minted this one through the management API, or `None`
    /// for a token minted any other way (ADR 0109).
    pub created_by_token_id: Option<Uuid>,
}

impl Token {
    /// Whether the token has been revoked (and so fails verification).
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }
}

/// What an attempt to revoke a token did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenRevocation {
    /// A still-active token was revoked.
    Revoked,
    /// The token exists and was revoked earlier. Nothing was changed, and the
    /// first revocation's timestamp stands.
    AlreadyRevoked,
    /// No token has that id.
    NotFound,
    /// Refused: revoking it would leave no surviving credential. Nothing was
    /// changed.
    LastCredential,
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
            created_by_token_id: row.try_get("created_by_token_id")?,
        })
    }
}

/// What a presented bearer token proved to be, from [`Store::verify_token`].
/// Carries no secret: only what an authorization decision needs to know about
/// the credential that made a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedToken {
    /// The token's row id.
    pub id: Uuid,
    /// When it stops verifying, or `None` for a token that never expires.
    pub expires_at: Option<DateTime<Utc>>,
    /// When the upstream sign-in behind this token's session completed, or
    /// `None` if it has none: a non-OAuth token, or an OAuth session that
    /// began before the column existed (ADR 0109).
    pub authenticated_at: Option<DateTime<Utc>>,
    /// The bound identity this token was minted for, if any.
    pub oauth_identity_id: Option<Uuid>,
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
const TOKEN_COLUMNS: &str = "id, label, created_at, last_used_at, revoked_at, expires_at, \
     provider, oauth_identity_id, client_id, created_by_token_id";

const BOOTSTRAP_LOCK_KEY: i64 = 0x4158_4f4e_424f_4f54;

/// Serializes every write that removes a credential (ADR 0109). The lockout
/// guard counts what a removal would leave behind; without one lock shared by
/// all removals, two concurrent requests each see the other's credential as the
/// survivor under `READ COMMITTED` and both commit.
const CREDENTIAL_LOCK_KEY: i64 = 0x4158_4f4e_4352_4544;

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
        self.insert_token(label, None).await
    }

    /// [`issue_token`](Self::issue_token), recording `created_by` as the token
    /// that asked for it: the management API's mint (ADR 0109). The new token
    /// never expires, exactly like a CLI-minted one.
    pub async fn issue_token_created_by(
        &self,
        label: &str,
        created_by: Uuid,
    ) -> Result<IssuedToken, StoreError> {
        self.insert_token(label, Some(created_by)).await
    }

    async fn insert_token(
        &self,
        label: &str,
        created_by: Option<Uuid>,
    ) -> Result<IssuedToken, StoreError> {
        let token = generate_token();
        let hash = hash_token(&token);
        let row = sqlx_core::query::query(
            "INSERT INTO tokens (label, hash, created_by_token_id) VALUES ($1, $2, $3) \
             RETURNING id",
        )
        .bind(label)
        .bind(&hash)
        .bind(created_by)
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
    /// unexpired** row, and (on a match) stamps `last_used_at`. Returns what the
    /// token is on success or `None` if the token is unknown, revoked, or
    /// expired. The match and the `last_used_at` touch happen in one
    /// statement so verification is a single round-trip on the hot path
    /// (every `/v1/` request goes through here). A CLI-minted token's
    /// `expires_at` is `NULL`, so `expires_at IS NULL` keeps it verifying
    /// forever exactly as before OAuth existed.
    pub async fn verify_token(&self, raw: &str) -> Result<Option<VerifiedToken>, StoreError> {
        let hash = hash_token(raw);
        let row = sqlx_core::query::query(
            "UPDATE tokens SET last_used_at = now() \
             WHERE hash = $1 AND revoked_at IS NULL \
               AND (expires_at IS NULL OR expires_at > now()) \
             RETURNING id, expires_at, authenticated_at, oauth_identity_id",
        )
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => Ok(Some(VerifiedToken {
                id: row.try_get("id")?,
                expires_at: row.try_get("expires_at")?,
                authenticated_at: row.try_get("authenticated_at")?,
                oauth_identity_id: row.try_get("oauth_identity_id")?,
            })),
            None => Ok(None),
        }
    }

    /// Mint an OAuth-backed access token: like [`issue_token`](Self::issue_token),
    /// but carrying an expiry and the OAuth provenance columns. Used by the
    /// `oauth` module's token orchestration, never by the CLI.
    ///
    /// `authenticated_at` is when the upstream provider says the owner behind
    /// this session authenticated: the verified identity token's time for a
    /// token minted from a sign-in, the refresh token's recorded time for one
    /// minted by a refresh. Never the caller's clock.
    pub async fn issue_oauth_token(
        &self,
        label: &str,
        expires_at: DateTime<Utc>,
        provider: &str,
        oauth_identity_id: Uuid,
        client_id: &str,
        authenticated_at: Option<DateTime<Utc>>,
    ) -> Result<IssuedToken, StoreError> {
        let token = generate_token();
        let hash = hash_token(&token);
        let row = sqlx_core::query::query(
            "INSERT INTO tokens \
                 (label, hash, expires_at, provider, oauth_identity_id, client_id, authenticated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
        )
        .bind(label)
        .bind(&hash)
        .bind(expires_at)
        .bind(provider)
        .bind(oauth_identity_id)
        .bind(client_id)
        .bind(authenticated_at)
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

    /// At most `limit` tokens for the management API's list (ADR 0109): the
    /// ones that still verify first, then everything else, newest first
    /// within each, with the id breaking ties so the cap cuts at the same
    /// row every time.
    ///
    /// The order is what makes the cap safe. Expired OAuth access tokens are
    /// never deleted and a signed-in client mints one an hour, so on a
    /// long-lived instance they outnumber everything else; a newest-first cap
    /// would eventually push the non-expiring tokens the owner actually needs
    /// to see off the end of the list. Nothing supports this order with an
    /// index, so it sorts the table; pruning the expired rows is #635.
    pub async fn list_tokens_live_first(&self, limit: i64) -> Result<Vec<Token>, StoreError> {
        let sql = format!(
            "SELECT {TOKEN_COLUMNS} FROM tokens \
             ORDER BY (revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now())) DESC, \
                      created_at DESC, id \
             LIMIT $1"
        );
        let tokens = sqlx_core::query_as::query_as::<Postgres, Token>(&sql)
            .bind(limit)
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
    /// timestamp is preserved.
    ///
    /// Revoking an OAuth session's access token also revokes the refresh
    /// tokens of that identity and client, so the session cannot renew itself
    /// (see [`end_sessions_in_tx`](Self::end_sessions_in_tx) for how wide
    /// that cut is).
    ///
    /// Takes the credential lock, like every credential-removing write, so a
    /// guarded removal elsewhere never counts this token as a survivor while
    /// it is being revoked.
    pub async fn revoke_token(&self, id: Uuid) -> Result<bool, StoreError> {
        Ok(self.remove_token(id, None).await? == TokenRevocation::Revoked)
    }

    /// [`revoke_token`](Self::revoke_token), telling an earlier revocation
    /// apart from an unknown id: the management API's revoke once the owner
    /// has confirmed `allow_lockout`.
    pub async fn revoke_token_allowing_lockout(
        &self,
        id: Uuid,
    ) -> Result<TokenRevocation, StoreError> {
        self.remove_token(id, None).await
    }

    /// [`revoke_token`](Self::revoke_token), refused when it would leave the
    /// owner with no way to sign in (ADR 0109): no other active non-expiring
    /// token, and no bound identity whose provider is in `usable_providers`.
    /// A refusal rolls everything back and revokes nothing.
    ///
    /// Only a token that is itself a surviving credential, an active one with
    /// no expiry, can be the last. Revoking an OAuth access token is never
    /// refused: it was never a way back in, so its removal cannot be what
    /// locks the owner out, even on an instance that has no survivor already.
    ///
    /// The management API's revoke. The CLI keeps the unguarded form: an
    /// operator with a shell can always mint another token.
    pub async fn revoke_token_unless_last_credential(
        &self,
        id: Uuid,
        usable_providers: &[String],
    ) -> Result<TokenRevocation, StoreError> {
        self.remove_token(id, Some(usable_providers)).await
    }

    async fn remove_token(
        &self,
        id: Uuid,
        guard: Option<&[String]>,
    ) -> Result<TokenRevocation, StoreError> {
        let mut tx = self.pool.begin().await?;
        // Bound the wait for the credential lock, as identity removal does.
        sqlx_core::query::query("SET LOCAL lock_timeout = '5s'")
            .execute(&mut *tx)
            .await?;
        Self::lock_credentials(&mut tx).await?;
        let Some(row) = sqlx_core::query::query(
            "SELECT revoked_at IS NOT NULL AS revoked, expires_at IS NULL AS non_expiring, \
                    oauth_identity_id, client_id \
               FROM tokens WHERE id = $1 FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        else {
            tx.rollback().await?;
            return Ok(TokenRevocation::NotFound);
        };
        if row.try_get::<bool, _>("revoked")? {
            tx.rollback().await?;
            return Ok(TokenRevocation::AlreadyRevoked);
        }
        sqlx_core::query::query("UPDATE tokens SET revoked_at = now() WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        // An OAuth session's access token is half of the session. Left alone,
        // its refresh token would mint a replacement on the client's next
        // request and the revoke would have ended nothing.
        let identity: Option<Uuid> = row.try_get("oauth_identity_id")?;
        let client: Option<String> = row.try_get("client_id")?;
        if let (Some(identity), Some(client)) = (identity, client) {
            Self::end_sessions_in_tx(&mut tx, identity, &client).await?;
        }
        // Counted on the state this transaction would commit, and only when
        // this token was a survivor to begin with.
        if let Some(usable_providers) = guard {
            if row.try_get::<bool, _>("non_expiring")?
                && !Self::surviving_credential_in_tx(&mut tx, usable_providers).await?
            {
                tx.rollback().await?;
                return Ok(TokenRevocation::LastCredential);
            }
        }
        tx.commit().await?;
        Ok(TokenRevocation::Revoked)
    }

    /// Revoke every active refresh token for one identity and client, so no
    /// session of that client can renew itself.
    ///
    /// Nothing ties an access token to the one refresh chain that minted it,
    /// only to its identity and client, so this is the narrowest cut there
    /// is: it ends every session that client holds for that identity, which
    /// is also what refresh-token reuse detection does. Access tokens those
    /// sessions already hold are not touched and run out on their own, within
    /// the access-token lifetime.
    ///
    /// Two statements on purpose. A rotation in flight holds its old row's
    /// lock and has already inserted the replacement, which a single `UPDATE`
    /// would wait for and then not see. Locking the rows first does the
    /// waiting; the `UPDATE` then runs on a snapshot that includes whatever
    /// that rotation committed.
    async fn end_sessions_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        oauth_identity_id: Uuid,
        client_id: &str,
    ) -> Result<(), StoreError> {
        for sql in [
            "SELECT id FROM oauth_refresh_tokens \
              WHERE oauth_identity_id = $1 AND client_id = $2 AND revoked_at IS NULL \
                FOR UPDATE",
            "UPDATE oauth_refresh_tokens SET revoked_at = now() \
              WHERE oauth_identity_id = $1 AND client_id = $2 AND revoked_at IS NULL",
        ] {
            sqlx_core::query::query(sql)
                .bind(oauth_identity_id)
                .bind(client_id)
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
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
        authenticated_at: Option<DateTime<Utc>>,
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
        let pair = Self::mint_oauth_pair_in_tx(
            &mut tx,
            provider,
            identity_id,
            client_id,
            access_expires_at,
            refresh_expires_at,
            authenticated_at,
        )
        .await?;
        tx.commit().await?;
        Ok(Some(pair))
    }

    pub(crate) async fn mint_oauth_pair_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        provider: &str,
        identity_id: Uuid,
        client_id: &str,
        access_expires_at: DateTime<Utc>,
        refresh_expires_at: DateTime<Utc>,
        // What the verified identity token says about when the owner
        // authenticated, never this function's own clock. One value for both
        // rows: the refresh chain must carry exactly what the access token
        // shows.
        authenticated_at: Option<DateTime<Utc>>,
    ) -> Result<IssuedOAuthTokenPair, StoreError> {
        let access_token = generate_token();
        let access_hash = hash_token(&access_token);
        sqlx_core::query::query(
            "INSERT INTO tokens \
                 (label, hash, expires_at, provider, oauth_identity_id, client_id, authenticated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(format!("oauth:{provider}:{client_id}"))
        .bind(&access_hash)
        .bind(access_expires_at)
        .bind(provider)
        .bind(identity_id)
        .bind(client_id)
        .bind(authenticated_at)
        .execute(&mut **tx)
        .await?;

        let refresh_token = generate_refresh_token();
        let refresh_hash = hash_token(&refresh_token);
        sqlx_core::query::query(
            "INSERT INTO oauth_refresh_tokens \
                 (hash, oauth_identity_id, client_id, expires_at, authenticated_at) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&refresh_hash)
        .bind(identity_id)
        .bind(client_id)
        .bind(refresh_expires_at)
        .bind(authenticated_at)
        .execute(&mut **tx)
        .await?;

        Ok(IssuedOAuthTokenPair {
            access_token,
            refresh_token,
        })
    }

    /// Take the transaction-scoped credential lock. See [`CREDENTIAL_LOCK_KEY`].
    pub(crate) async fn lock_credentials(
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query("SELECT pg_advisory_xact_lock($1)")
            .bind(CREDENTIAL_LOCK_KEY)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// Whether the owner could still sign in, given the state this transaction
    /// would commit (ADR 0109). A surviving credential is an active token that
    /// never expires, or a bound identity whose provider is in
    /// `usable_providers`, the providers a sign-in could currently go through.
    /// An expiring OAuth access token is never one, however long it has left.
    ///
    /// Call it after the removal's own writes and under
    /// [`lock_credentials`](Self::lock_credentials), so the count is of what
    /// would be left and no concurrent removal can change it.
    pub(crate) async fn surviving_credential_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        usable_providers: &[String],
    ) -> Result<bool, StoreError> {
        let survives: bool = sqlx_core::query::query(
            "SELECT EXISTS (SELECT 1 FROM tokens \
                             WHERE revoked_at IS NULL AND expires_at IS NULL) \
                 OR EXISTS (SELECT 1 FROM oauth_identities WHERE provider = ANY($1)) \
                 AS survives",
        )
        .bind(usable_providers)
        .fetch_one(&mut **tx)
        .await?
        .try_get("survives")?;
        Ok(survives)
    }

    pub(crate) async fn lock_bootstrap(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query("SELECT pg_advisory_xact_lock($1)")
            .bind(BOOTSTRAP_LOCK_KEY)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    pub(crate) async fn bootstrap_available_in_tx(
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
