//! DB-gated tests for the `oauth_refresh_tokens` opportunistic sweep
//! ([issue 291](https://github.com/) — the table retained every
//! rotated/revoked row forever).
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store --test oauth_refresh_tokens -- --ignored
//! ```

mod common;

use chrono::{Duration, Utc};
use common::migrated_store;
use sqlx_postgres::PgPool;
use uuid::Uuid;

/// Insert a refresh token row with fully explicit timestamps, bypassing the
/// `Store` API so the test can construct rows that are already stale without
/// waiting 30 real days.
async fn insert_row(
    pool: &PgPool,
    hash: &str,
    oauth_identity_id: Uuid,
    client_id: &str,
    expires_at: chrono::DateTime<Utc>,
    revoked_at: Option<chrono::DateTime<Utc>>,
) -> Uuid {
    let row = sqlx_core::query::query(
        "INSERT INTO oauth_refresh_tokens (hash, oauth_identity_id, client_id, expires_at, revoked_at) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(hash)
    .bind(oauth_identity_id)
    .bind(client_id)
    .bind(expires_at)
    .bind(revoked_at)
    .fetch_one(pool)
    .await
    .expect("insert row");
    sqlx_core::row::Row::try_get(&row, "id").expect("id")
}

async fn row_exists(pool: &PgPool, id: Uuid) -> bool {
    sqlx_core::query::query("SELECT 1 FROM oauth_refresh_tokens WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .expect("query")
        .is_some()
}

/// Delete an identity and (since `oauth_refresh_tokens.oauth_identity_id` has
/// no `ON DELETE CASCADE`) its refresh-token rows first, so repeated test
/// runs don't accumulate rows in the shared test database — mirrors
/// `common::cleanup_account`. Must run even for rows the sweep deliberately
/// spared (`fresh_revoked`, `live`), which otherwise survive forever.
async fn cleanup_identity(pool: &PgPool, oauth_identity_id: Uuid) {
    let _ =
        sqlx_core::query::query("DELETE FROM oauth_refresh_tokens WHERE oauth_identity_id = $1")
            .bind(oauth_identity_id)
            .execute(pool)
            .await;
    let _ = sqlx_core::query::query("DELETE FROM oauth_identities WHERE id = $1")
        .bind(oauth_identity_id)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn issuing_a_token_sweeps_long_dead_rows_but_spares_fresh_ones() {
    let store = migrated_store().await;
    let pool = store.pool().clone();
    let identity = store
        .bind_identity("google", &format!("sweep-issue-{}", Uuid::new_v4()), None)
        .await
        .expect("bind identity");

    let now = Utc::now();

    // Revoked long enough ago to be swept.
    let stale_revoked = insert_row(
        &pool,
        &format!("hash-stale-revoked-{}", Uuid::new_v4()),
        identity.id,
        "client-a",
        now + Duration::days(30),
        Some(now - Duration::days(31)),
    )
    .await;

    // Never redeemed, but its own expiry lapsed long enough ago to be swept.
    let stale_expired = insert_row(
        &pool,
        &format!("hash-stale-expired-{}", Uuid::new_v4()),
        identity.id,
        "client-a",
        now - Duration::days(31),
        None,
    )
    .await;

    // Revoked recently — inside the sweep window, must survive (reuse
    // detection still needs this tombstone).
    let fresh_revoked = insert_row(
        &pool,
        &format!("hash-fresh-revoked-{}", Uuid::new_v4()),
        identity.id,
        "client-a",
        now + Duration::days(30),
        Some(now - Duration::days(1)),
    )
    .await;

    // Still live and unexpired, must survive.
    let live = insert_row(
        &pool,
        &format!("hash-live-{}", Uuid::new_v4()),
        identity.id,
        "client-a",
        now + Duration::days(30),
        None,
    )
    .await;

    // Any call to issue_refresh_token opportunistically sweeps first.
    store
        .issue_refresh_token(
            &format!("hash-new-{}", Uuid::new_v4()),
            identity.id,
            "client-a",
            now + Duration::days(30),
            None,
        )
        .await
        .expect("issue");

    assert!(
        !row_exists(&pool, stale_revoked).await,
        "long-revoked row should be swept"
    );
    assert!(
        !row_exists(&pool, stale_expired).await,
        "long-expired never-revoked row should be swept"
    );
    assert!(
        row_exists(&pool, fresh_revoked).await,
        "recently-revoked row must survive within the sweep window"
    );
    assert!(row_exists(&pool, live).await, "live row must survive");

    cleanup_identity(&pool, identity.id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn redeeming_a_token_sweeps_long_dead_rows_even_when_the_presented_hash_is_unknown() {
    let store = migrated_store().await;
    let pool = store.pool().clone();
    let identity = store
        .bind_identity("google", &format!("sweep-redeem-{}", Uuid::new_v4()), None)
        .await
        .expect("bind identity");

    let now = Utc::now();
    let stale_revoked = insert_row(
        &pool,
        &format!("hash-stale-{}", Uuid::new_v4()),
        identity.id,
        "client-a",
        now + Duration::days(30),
        Some(now - Duration::days(45)),
    )
    .await;

    let outcome = store
        .redeem_refresh_token(
            "no-such-hash",
            Uuid::new_v4(),
            "irrelevant-new-hash",
            now + Duration::days(30),
        )
        .await
        .expect("redeem call succeeds");
    assert!(
        outcome.is_err(),
        "an unknown presented hash is rejected, not rotated"
    );

    assert!(
        !row_exists(&pool, stale_revoked).await,
        "redeem's opportunistic sweep runs even when the presented token doesn't match anything"
    );

    cleanup_identity(&pool, identity.id).await;
}
