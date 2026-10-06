//! PostgreSQL availability regression tests. Run against a disposable database:
//! DATABASE_URL=... cargo test -p axon-store --test database_timeouts -- --ignored --test-threads=1

mod common;

use std::time::{Duration, Instant};

use axon_core::{DatabaseConfig, DatabaseTimeouts};
use axon_store::Store;
use common::{cleanup_account, insert_message, raw_pool, test_account};
use sqlx_postgres::PgPool;
use uuid::Uuid;

async fn bounded_store(timeouts: DatabaseTimeouts) -> Store {
    Store::connect_with_config(&DatabaseConfig {
        url: std::env::var("DATABASE_URL").expect("DATABASE_URL"),
        max_connections: 1,
        timeouts,
    })
    .await
    .expect("bounded store")
}

async fn settings(pool: &PgPool) -> (i32, i32, i32) {
    sqlx_core::query_as::query_as(
        "SELECT \
         (SELECT setting::int FROM pg_settings WHERE name = 'statement_timeout'), \
         (SELECT setting::int FROM pg_settings WHERE name = 'lock_timeout'), \
         (SELECT setting::int FROM pg_settings WHERE name = 'idle_in_transaction_session_timeout')",
    )
    .fetch_one(pool)
    .await
    .expect("PostgreSQL deadlines")
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn ordinary_statements_and_connection_replacements_inherit_deadlines() {
    let store = bounded_store(DatabaseTimeouts {
        statement_secs: 1,
        lock_secs: 2,
        idle_transaction_secs: 3,
        ..Default::default()
    })
    .await;
    assert_eq!(settings(store.pool()).await, (1000, 2000, 3000));
    let error = tokio::time::timeout(Duration::from_secs(3), async {
        sqlx_core::query::query("SELECT pg_sleep(10)")
            .execute(store.pool())
            .await
    })
    .await
    .expect("server-side statement deadline")
    .expect_err("query canceled by PostgreSQL");
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("57014")
    );
    assert_eq!(settings(store.pool()).await, (1000, 2000, 3000));
    store.pool().acquire().await.unwrap().close().await.unwrap();
    assert_eq!(settings(store.pool()).await, (1000, 2000, 3000));
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn pool_waits_and_bootstrap_lock_waits_are_bounded() {
    let store = bounded_store(DatabaseTimeouts {
        statement_secs: 3,
        lock_secs: 1,
        acquire_secs: 1,
        ..Default::default()
    })
    .await;
    let held = store.pool().acquire().await.expect("take only connection");
    let started = Instant::now();
    let error = store.list_accounts().await.expect_err("pool unavailable");
    assert_eq!(error.diagnostic_reason(), "pool_timeout");
    assert!(started.elapsed() < Duration::from_secs(3));
    drop(held);

    // The real advisory lock used by web bootstrap, held by another session.
    let pool = raw_pool().await;
    let mut blocker = pool.begin().await.unwrap();
    sqlx_core::query::query("SELECT pg_advisory_xact_lock($1)")
        .bind(0x4158_4f4e_424f_4f54_i64)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let started = Instant::now();
    let error = store
        .issue_first_bootstrap_token("deadline regression")
        .await
        .expect_err("bootstrap lock held");
    assert_eq!(error.diagnostic_reason(), "database_lock_timeout");
    assert!(started.elapsed() < Duration::from_secs(3));
    store
        .list_accounts()
        .await
        .expect("ordinary pool recovered");
    blocker.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn bulk_jobs_use_one_separate_connection_and_a_longer_deadline() {
    let store = bounded_store(DatabaseTimeouts {
        statement_secs: 1,
        lock_secs: 4,
        acquire_secs: 1,
        maintenance_statement_secs: 3,
        ..Default::default()
    })
    .await;
    let pool = raw_pool().await;
    let account = test_account(&store, "bulk-deadline").await;
    let room = format!("!bulk-{}:localhost", Uuid::new_v4());
    let event_id = insert_message(&store, account, &room, 1, "fixture").await;
    let token = store
        .issue_token("bulk responsiveness fixture")
        .await
        .unwrap();
    let mut blocker = pool.begin().await.unwrap();
    sqlx_core::query::query("LOCK TABLE events IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let bulk_store = store.clone();
    let bulk_room = room.clone();
    let started = Instant::now();
    let bulk = tokio::spawn(async move { bulk_store.purge_room(account, &bulk_room).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let (waiting,): (bool,) = sqlx_core::query_as::query_as(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
                 WHERE application_name = 'axon-maintenance' AND state = 'active' \
                   AND wait_event_type = 'Lock')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("bulk query reached its own backend");

    tokio::time::timeout(Duration::from_millis(500), async {
        store.list_accounts().await.unwrap();
        assert!(store.verify_token(&token.token).await.unwrap().is_some());
    })
    .await
    .expect("auth and ordinary reads stay responsive during bulk work");
    let second = store
        .purge_room(account, &room)
        .await
        .expect_err("single bulk connection occupied");
    assert_eq!(second.diagnostic_reason(), "pool_timeout");
    let error = tokio::time::timeout(Duration::from_secs(5), bulk)
        .await
        .unwrap()
        .unwrap()
        .expect_err("bulk statement bounded");
    assert_eq!(error.diagnostic_reason(), "database_query_canceled");
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert!(started.elapsed() < Duration::from_secs(5));
    blocker.rollback().await.unwrap();
    // Timeout rolled the atomic purge back, and the maintenance pool recovers.
    assert!(store.get_event(account, &event_id).await.unwrap().is_some());
    store.purge_room(account, &room).await.expect("retry purge");
    store.revoke_token(token.id).await.unwrap();
    cleanup_account(&pool, account).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn abandoned_idle_transactions_release_their_locks() {
    let store = bounded_store(DatabaseTimeouts {
        idle_transaction_secs: 1,
        ..Default::default()
    })
    .await;
    let mut tx = store.pool().begin().await.unwrap();
    sqlx_core::query::query("SELECT 1")
        .execute(&mut *tx)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert!(sqlx_core::query::query("SELECT 1")
        .execute(&mut *tx)
        .await
        .is_err());
    drop(tx);
    store
        .list_accounts()
        .await
        .expect("terminated backend replaced");
    assert_eq!(settings(store.pool()).await.2, 1000);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn outbox_pruning_spans_batches_and_preserves_unprocessed_rows() {
    let store = bounded_store(DatabaseTimeouts::default()).await;
    let pool = raw_pool().await;
    let account = Uuid::new_v4();
    sqlx_core::query::query(
        "INSERT INTO search_outbox (account_id, event_id) SELECT $1, 'fixture-' || n \
         FROM generate_series(1, 2505) n",
    )
    .bind(account)
    .execute(&pool)
    .await
    .unwrap();
    let (through,): (i64,) = sqlx_core::query_as::query_as(
        "SELECT seq FROM search_outbox WHERE account_id = $1 ORDER BY seq OFFSET 2499 LIMIT 1",
    )
    .bind(account)
    .fetch_one(&pool)
    .await
    .unwrap();
    store.prune_search_outbox(through).await.unwrap();
    let (remaining,): (i64,) =
        sqlx_core::query_as::query_as("SELECT count(*) FROM search_outbox WHERE account_id = $1")
            .bind(account)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(remaining, 5);
    store.prune_search_outbox(through).await.unwrap();
    sqlx_core::query::query("DELETE FROM search_outbox WHERE account_id = $1")
        .bind(account)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn concurrent_slow_status_reads_cannot_exhaust_auth_and_sync_connections() {
    let store = bounded_store(DatabaseTimeouts {
        statement_secs: 3,
        lock_secs: 4,
        acquire_secs: 1,
        ..Default::default()
    })
    .await;
    let pool = raw_pool().await;
    let token = store.issue_token("read isolation fixture").await.unwrap();
    let (read_pid,): (i32,) = sqlx_core::query_as::query_as("SELECT pg_backend_pid()")
        .fetch_one(store.read_pool())
        .await
        .unwrap();
    let mut blocker = pool.begin().await.unwrap();
    sqlx_core::query::query("LOCK TABLE room_summaries IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let first_store = store.clone();
    let first = tokio::spawn(async move { first_store.backfill_progress().await });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let (waiting,): (bool,) = sqlx_core::query_as::query_as(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1 \
                 AND state = 'active' AND wait_event_type = 'Lock')",
            )
            .bind(read_pid)
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("status query running on read backend");
    let mut others = Vec::new();
    for _ in 0..4 {
        let caller = store.clone();
        others.push(tokio::spawn(
            async move { caller.backfill_progress().await },
        ));
    }
    tokio::time::timeout(Duration::from_millis(500), async {
        assert!(store.verify_token(&token.token).await.unwrap().is_some());
        store.list_accounts().await.unwrap();
    })
    .await
    .expect("ordinary pool remains responsive");
    for caller in others {
        assert_eq!(
            caller.await.unwrap().unwrap_err().diagnostic_reason(),
            "pool_timeout"
        );
    }
    assert_eq!(
        first.await.unwrap().unwrap_err().diagnostic_reason(),
        "database_query_canceled"
    );
    blocker.rollback().await.unwrap();
    store
        .backfill_progress()
        .await
        .expect("read pool recovered");
    store.revoke_token(token.id).await.unwrap();
}
