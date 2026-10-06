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

async fn wait_for_sweep(pool: &PgPool, id: Uuid) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while row_exists(pool, id).await {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("background sweep removes stale row");
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

    // Issuance schedules cleanup without waiting for the maintenance statement.
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

    wait_for_sweep(&pool, stale_revoked).await;
    wait_for_sweep(&pool, stale_expired).await;

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

    wait_for_sweep(&pool, stale_revoked).await;

    assert!(
        !row_exists(&pool, stale_revoked).await,
        "redeem's opportunistic sweep runs even when the presented token doesn't match anything"
    );

    cleanup_identity(&pool, identity.id).await;
}

/// A blocked cleanup cannot delay issuance/rotation or queue a sweep per caller.
/// Failures preserve tombstones, and the cooldown is shared by store clones.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn blocked_background_sweep_does_not_block_auth_and_respects_shared_cooldown() {
    use std::time::Duration as Wait;

    let store = axon_store::Store::connect_with_config(&axon_core::DatabaseConfig {
        url: std::env::var("DATABASE_URL").expect("DATABASE_URL"),
        max_connections: 1,
        timeouts: axon_core::DatabaseTimeouts {
            statement_secs: 3,
            lock_secs: 1,
            ..Default::default()
        },
    })
    .await
    .unwrap();
    let pool = common::raw_pool().await;
    let identity = store
        .bind_identity("google", &format!("blocked-sweep-{}", Uuid::new_v4()), None)
        .await
        .unwrap();
    let stale = insert_row(
        &pool,
        &format!("stale-{}", Uuid::new_v4()),
        identity.id,
        "fixture",
        Utc::now() + Duration::days(1),
        Some(Utc::now() - Duration::days(31)),
    )
    .await;
    let mut blocker = pool.begin().await.unwrap();
    sqlx_core::query::query("SELECT id FROM oauth_refresh_tokens WHERE id = $1 FOR UPDATE")
        .bind(stale)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    let account = common::test_account(&store, "index-sweep-isolation").await;
    common::insert_message(&store, account, "!index-sweep:localhost", 1, "seed fixture").await;
    let issued_hash = format!("fresh-{}", Uuid::new_v4());
    tokio::time::timeout(
        Wait::from_millis(500),
        store.issue_refresh_token(
            &issued_hash,
            identity.id,
            "fixture",
            Utc::now() + Duration::days(1),
            None,
        ),
    )
    .await
    .expect("cleanup does not delay issuance")
    .unwrap();
    let (backend,): (i32,) = tokio::time::timeout(Wait::from_secs(2), async {
        loop {
            let backend = sqlx_core::query_as::query_as::<_, (i32,)>(
                "SELECT pid FROM pg_stat_activity WHERE application_name = 'axon-maintenance' \
                 AND state = 'active' AND wait_event_type = 'Lock' LIMIT 1",
            )
            .fetch_optional(&pool)
            .await
            .unwrap();
            if let Some(backend) = backend {
                break backend;
            }
            tokio::time::sleep(Wait::from_millis(20)).await;
        }
    })
    .await
    .expect("cleanup is blocked in PostgreSQL");
    let (stale_hash,): (String,) =
        sqlx_core::query_as::query_as("SELECT hash FROM oauth_refresh_tokens WHERE id = $1")
            .bind(stale)
            .fetch_one(&pool)
            .await
            .unwrap();
    let result = store
        .redeem_refresh_token(
            &stale_hash,
            Uuid::new_v4(),
            &format!("unused-{}", Uuid::new_v4()),
            Utc::now() + Duration::days(1),
        )
        .await
        .unwrap();
    assert!(
        matches!(result, Err(axon_store::RedeemRefreshTokenError::NotFound)),
        "old tombstones outside retention never revoke current sessions"
    );
    tokio::time::timeout(Wait::from_millis(500), async {
        assert!(!store.events_for_index(0, 100).await.unwrap().is_empty());
        store.prune_search_outbox(0).await.unwrap();
    })
    .await
    .expect("seed and prune do not queue behind maintenance cleanup");
    for attempt in 0..3 {
        let result = tokio::time::timeout(
            Wait::from_millis(500),
            store.clone().redeem_refresh_token(
                &issued_hash,
                Uuid::new_v4(),
                &format!("replacement-{}", Uuid::new_v4()),
                Utc::now() + Duration::days(1),
            ),
        )
        .await
        .expect("cleanup does not delay rotation")
        .unwrap();
        // First rotation succeeds; later reuse exercises the revocation path.
        if attempt == 0 {
            assert!(result.is_ok());
        } else {
            assert!(matches!(
                result,
                Err(axon_store::RedeemRefreshTokenError::Reused)
            ));
        }
    }
    tokio::time::timeout(Wait::from_secs(3), async {
        loop {
            let (active,): (bool,) = sqlx_core::query_as::query_as(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1 AND state = 'active')",
            ).bind(backend).fetch_one(&pool).await.unwrap();
            if !active { break; }
            tokio::time::sleep(Wait::from_millis(20)).await;
        }
    }).await.expect("cleanup lock deadline fires");
    assert!(
        row_exists(&pool, stale).await,
        "failed cleanup preserves stale rows"
    );
    blocker.rollback().await.unwrap();
    store
        .clone()
        .issue_refresh_token(
            &format!("cooldown-{}", Uuid::new_v4()),
            identity.id,
            "fixture",
            Utc::now() + Duration::days(1),
            None,
        )
        .await
        .unwrap();
    // Once background admission is available again, an explicit sweep removes
    // the row. If the clone queued another sweep during cooldown, the count
    // would be zero instead of one.
    assert_eq!(store.delete_stale_refresh_tokens().await.unwrap(), 1);
    common::cleanup_account(&pool, account).await;
    cleanup_identity(&pool, identity.id).await;
}
