//! Session ids on bearer tokens and refresh tokens.
//!
//! An OAuth sign-in shares one id across the access token and the refresh
//! token. Rotation copies it forward. A non-expiring credential's session is
//! its own row id, including when a writer omits the column and the insert
//! trigger fills it.
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon \
//!   cargo test -p axon-store --test credential_sessions -- --ignored
//! ```

mod common;

use axon_store::{IdentityRedemption, OAuthSession};
use chrono::{Duration, Utc};
use common::{migrated_store, raw_pool};
use sqlx_core::row::Row;
use sqlx_postgres::PgPool;
use uuid::Uuid;

async fn token_session(pool: &PgPool, id: Uuid) -> Uuid {
    sqlx_core::query::query("SELECT session_id FROM tokens WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("token row")
        .try_get("session_id")
        .expect("session_id")
}

async fn refresh_session(pool: &PgPool, id: Uuid) -> Uuid {
    sqlx_core::query::query("SELECT session_id FROM oauth_refresh_tokens WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("refresh row")
        .try_get("session_id")
        .expect("session_id")
}

async fn delete_token(pool: &PgPool, id: Uuid) {
    sqlx_core::query::query("DELETE FROM tokens WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .expect("delete token");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_non_expiring_credential_uses_its_row_id_as_its_session() {
    let store = migrated_store().await;
    let pool = raw_pool().await;

    let issued = store.issue_token("session-cli").await.expect("issue");
    assert_eq!(token_session(&pool, issued.id).await, issued.id);
    let verified = store
        .verify_token(&issued.token)
        .await
        .expect("verify")
        .expect("verifies");
    assert_eq!(verified.session_id, issued.id);

    let child = store
        .issue_token_created_by("session-child", issued.id)
        .await
        .expect("issue child");
    assert_eq!(token_session(&pool, child.id).await, child.id);
    assert_ne!(child.id, issued.id);

    delete_token(&pool, child.id).await;
    delete_token(&pool, issued.id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn an_insert_that_omits_session_id_gets_the_row_id() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let identity = store
        .bind_identity("google", &format!("omit-{}", Uuid::new_v4()), None)
        .await
        .expect("bind");

    let token_id: Uuid = sqlx_core::query::query(
        "INSERT INTO tokens (label, hash) VALUES ($1, $2) RETURNING id, session_id",
    )
    .bind("omitted")
    .bind(format!("hash-{}", Uuid::new_v4()))
    .fetch_one(&pool)
    .await
    .expect("insert token")
    .try_get("id")
    .expect("id");
    assert_eq!(token_session(&pool, token_id).await, token_id);

    let refresh_id: Uuid = sqlx_core::query::query(
        "INSERT INTO oauth_refresh_tokens (hash, oauth_identity_id, client_id, expires_at) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(format!("hash-{}", Uuid::new_v4()))
    .bind(identity.id)
    .bind("web")
    .bind(Utc::now() + Duration::days(1))
    .fetch_one(&pool)
    .await
    .expect("insert refresh")
    .try_get("id")
    .expect("id");
    assert_eq!(refresh_session(&pool, refresh_id).await, refresh_id);

    delete_token(&pool, token_id).await;
    store.delete_identity(identity.id).await.expect("cleanup");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn an_insert_that_supplies_session_id_keeps_it() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let identity = store
        .bind_identity("google", &format!("keep-{}", Uuid::new_v4()), None)
        .await
        .expect("bind");
    let session = Uuid::new_v4();

    let token_id: Uuid = sqlx_core::query::query(
        "INSERT INTO tokens (label, hash, session_id) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind("supplied")
    .bind(format!("hash-{}", Uuid::new_v4()))
    .bind(session)
    .fetch_one(&pool)
    .await
    .expect("insert token")
    .try_get("id")
    .expect("id");
    assert_ne!(token_id, session);
    assert_eq!(token_session(&pool, token_id).await, session);

    let refresh_id: Uuid = sqlx_core::query::query(
        "INSERT INTO oauth_refresh_tokens \
             (hash, oauth_identity_id, client_id, expires_at, session_id) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(format!("hash-{}", Uuid::new_v4()))
    .bind(identity.id)
    .bind("web")
    .bind(Utc::now() + Duration::days(1))
    .bind(session)
    .fetch_one(&pool)
    .await
    .expect("insert refresh")
    .try_get("id")
    .expect("id");
    assert_ne!(refresh_id, session);
    assert_eq!(refresh_session(&pool, refresh_id).await, session);

    delete_token(&pool, token_id).await;
    store.delete_identity(identity.id).await.expect("cleanup");
}

/// The migration's own backfill statements.
///
/// Each one is a single line, which is how
/// `20261013180000_credential_session_id.sql` writes them. The test runs
/// those lines so a change to the migration's backfill is what gets executed.
fn migration_backfill_statements() -> Vec<&'static str> {
    let updates: Vec<&str> = include_str!("../migrations/20261013180000_credential_session_id.sql")
        .lines()
        .map(str::trim)
        .filter(|line| {
            let upper = line.to_ascii_uppercase();
            upper.starts_with("UPDATE ") && upper.contains("SET SESSION_ID = ID")
        })
        .collect();
    assert!(
        updates.iter().any(|sql| sql.starts_with("UPDATE tokens ")),
        "migration backfill for tokens missing: {updates:?}"
    );
    assert!(
        updates
            .iter()
            .any(|sql| sql.starts_with("UPDATE oauth_refresh_tokens ")),
        "migration backfill for refresh tokens missing: {updates:?}"
    );
    updates
}

/// Runs the migration file's backfill against rows shaped like ones written
/// before the column existed: `session_id` null, and the insert trigger off,
/// inside a transaction that rolls the schema back.
///
/// `DISABLE TRIGGER` and `DROP NOT NULL` take `ACCESS EXCLUSIVE` locks on
/// `tokens` and `oauth_refresh_tokens`. The transaction sets
/// `lock_timeout = '3s'` and rolls back, so a lock held by another connection
/// fails this test instead of waiting it out. Run the suite with
/// `--test-threads=1`, as the other Postgres suites do.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn legacy_rows_backfill_to_their_own_id() {
    let backfill = migration_backfill_statements();
    let store = migrated_store().await;
    let pool = store.pool().clone();
    let mut tx = pool.begin().await.expect("begin");
    sqlx_core::query::query("SET LOCAL lock_timeout = '3s'")
        .execute(&mut *tx)
        .await
        .expect("lock timeout");
    for (table, trigger) in [
        ("tokens", "tokens_set_session_id"),
        (
            "oauth_refresh_tokens",
            "oauth_refresh_tokens_set_session_id",
        ),
    ] {
        sqlx_core::query::query(&format!("ALTER TABLE {table} DISABLE TRIGGER {trigger}"))
            .execute(&mut *tx)
            .await
            .expect("disable trigger");
        sqlx_core::query::query(&format!(
            "ALTER TABLE {table} ALTER COLUMN session_id DROP NOT NULL"
        ))
        .execute(&mut *tx)
        .await
        .expect("drop not null");
    }

    let legacy_token: Uuid =
        sqlx_core::query::query("INSERT INTO tokens (label, hash) VALUES ($1, $2) RETURNING id")
            .bind("legacy")
            .bind(format!("hash-{}", Uuid::new_v4()))
            .fetch_one(&mut *tx)
            .await
            .expect("insert legacy token")
            .try_get("id")
            .expect("id");
    let kept = Uuid::new_v4();
    let kept_token: Uuid = sqlx_core::query::query(
        "INSERT INTO tokens (label, hash, session_id) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind("already-set")
    .bind(format!("hash-{}", Uuid::new_v4()))
    .bind(kept)
    .fetch_one(&mut *tx)
    .await
    .expect("insert kept token")
    .try_get("id")
    .expect("id");

    let identity: Uuid = sqlx_core::query::query(
        "INSERT INTO oauth_identities (provider, subject) VALUES ('google', $1) RETURNING id",
    )
    .bind(format!("legacy-{}", Uuid::new_v4()))
    .fetch_one(&mut *tx)
    .await
    .expect("insert identity")
    .try_get("id")
    .expect("id");
    let legacy_refresh: Uuid = sqlx_core::query::query(
        "INSERT INTO oauth_refresh_tokens (hash, oauth_identity_id, client_id, expires_at) \
         VALUES ($1, $2, 'web', now() + interval '1 day') RETURNING id",
    )
    .bind(format!("hash-{}", Uuid::new_v4()))
    .bind(identity)
    .fetch_one(&mut *tx)
    .await
    .expect("insert legacy refresh")
    .try_get("id")
    .expect("id");

    let legacy_token_session: Option<Uuid> =
        sqlx_core::query::query("SELECT session_id FROM tokens WHERE id = $1")
            .bind(legacy_token)
            .fetch_one(&mut *tx)
            .await
            .expect("read legacy token")
            .try_get("session_id")
            .expect("session_id");
    assert_eq!(
        legacy_token_session, None,
        "a pre-trigger row has no session yet"
    );

    for sql in backfill {
        sqlx_core::query::query(sql)
            .execute(&mut *tx)
            .await
            .expect("migration backfill");
    }

    let filled: Uuid = sqlx_core::query::query("SELECT session_id FROM tokens WHERE id = $1")
        .bind(legacy_token)
        .fetch_one(&mut *tx)
        .await
        .expect("read backfilled token")
        .try_get("session_id")
        .expect("session_id");
    let unchanged: Uuid = sqlx_core::query::query("SELECT session_id FROM tokens WHERE id = $1")
        .bind(kept_token)
        .fetch_one(&mut *tx)
        .await
        .expect("read kept token")
        .try_get("session_id")
        .expect("session_id");
    let filled_refresh: Uuid =
        sqlx_core::query::query("SELECT session_id FROM oauth_refresh_tokens WHERE id = $1")
            .bind(legacy_refresh)
            .fetch_one(&mut *tx)
            .await
            .expect("read backfilled refresh")
            .try_get("session_id")
            .expect("session_id");
    assert_eq!(filled, legacy_token);
    assert_eq!(unchanged, kept);
    assert_eq!(filled_refresh, legacy_refresh);

    tx.rollback().await.expect("restore schema");

    let issued = store
        .issue_token("after-backfill")
        .await
        .expect("trigger still assigns a session");
    assert_eq!(token_session(&pool, issued.id).await, issued.id);
    delete_token(&pool, issued.id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn an_oauth_pair_minted_in_one_transaction_shares_a_session() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let subject = format!("pair-{}", Uuid::new_v4());
    let identity = store
        .bind_identity("google", &subject, None)
        .await
        .expect("bind");
    let pair = store
        .redeem_identity_atomically(
            &IdentityRedemption {
                provider: "google",
                subject: &subject,
                email: None,
                replay_key: &Uuid::new_v4().to_string(),
                client_id: "session-test",
                access_expires_at: Utc::now() + Duration::hours(1),
                refresh_expires_at: Utc::now() + Duration::days(30),
                authenticated_at: Some(Utc::now()),
            },
            None,
        )
        .await
        .expect("redeem")
        .expect("identity is bound");

    let access = store
        .verify_token(&pair.access_token)
        .await
        .expect("verify")
        .expect("access token verifies");
    let refresh_id: Uuid =
        sqlx_core::query::query("SELECT id FROM oauth_refresh_tokens WHERE hash = $1")
            .bind(axon_core::hash_secret(&pair.refresh_token))
            .fetch_one(&pool)
            .await
            .expect("refresh row")
            .try_get("id")
            .expect("id");
    let shared = refresh_session(&pool, refresh_id).await;
    assert_eq!(access.session_id, shared);
    assert_ne!(access.session_id, access.id);
    assert_ne!(shared, refresh_id);

    store.delete_identity(identity.id).await.expect("cleanup");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn refresh_rotation_copies_the_session_onto_the_new_rows() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let identity = store
        .bind_identity("google", &format!("rotate-{}", Uuid::new_v4()), None)
        .await
        .expect("bind");
    let session = Uuid::new_v4();
    let first_hash = format!("hash-{}", Uuid::new_v4());
    store
        .issue_refresh_token(
            &first_hash,
            identity.id,
            "web",
            Utc::now() + Duration::days(30),
            OAuthSession {
                session_id: session,
                authenticated_at: Some(Utc::now()),
            },
        )
        .await
        .expect("issue refresh");

    let new_hash = format!("hash-{}", Uuid::new_v4());
    let rotated = store
        .redeem_refresh_token(
            &first_hash,
            Uuid::new_v4(),
            &new_hash,
            Utc::now() + Duration::days(30),
        )
        .await
        .expect("redeem")
        .expect("rotation");
    assert_eq!(rotated.session_id, session);
    assert_eq!(refresh_session(&pool, rotated.id).await, session);
    assert_ne!(rotated.session_id, rotated.id);

    let access = store
        .issue_oauth_token(
            "rotated",
            Utc::now() + Duration::hours(1),
            "google",
            identity.id,
            "web",
            OAuthSession {
                session_id: rotated.session_id,
                authenticated_at: Some(Utc::now()),
            },
        )
        .await
        .expect("issue access");
    let verified = store
        .verify_token(&access.token)
        .await
        .expect("verify")
        .expect("verifies");
    assert_eq!(verified.session_id, session);
    assert_ne!(verified.id, session);

    store.delete_identity(identity.id).await.expect("cleanup");
}
