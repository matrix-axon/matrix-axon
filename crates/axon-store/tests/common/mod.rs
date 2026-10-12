//! Shared helpers for the DB-gated store integration tests.
//!
//! Each integration test binary pulls this in with `mod common;`. Not every
//! binary uses every helper, so dead-code is allowed here.
#![allow(dead_code)]

use axon_store::{NewEvent, Store};
use sqlx_postgres::{PgPool, PgPoolOptions};
use uuid::Uuid;

fn db_url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests")
}

/// Provision a throwaway account with a unique random user id (so concurrent or
/// repeated runs never collide), returning its `account_id`. `prefix` just makes
/// the generated user id readable in the DB during debugging.
pub async fn test_account(store: &Store, prefix: &str) -> Uuid {
    let user = format!("@{prefix}-{}:localhost", Uuid::new_v4());
    store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("account")
        .account_id
}

/// Insert a decrypted `m.room.message` with a text body, returning its event_id.
pub async fn insert_message(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    origin_ts: i64,
    body: &str,
) -> String {
    let event_id = format!("$evt-{}:localhost", Uuid::new_v4());
    let content = serde_json::json!({ "msgtype": "m.text", "body": body });
    store
        .upsert_event(&NewEvent {
            event_id: &event_id,
            room_id,
            account_id,
            sender: "@alice:localhost",
            origin_ts,
            event_type: "m.room.message",
            content: Some(content.clone()),
            raw_event: serde_json::json!({ "type": "m.room.message", "content": content }),
            megolm_session_id: None,
            redacts: None,
            relates_to: None,
            decrypted_body_text: Some(body),
        })
        .await
        .expect("insert message");
    event_id
}

/// A migrated `Store` connected to the test database. Named for the surprising
/// part — it runs migrations, not just opens a connection.
pub async fn migrated_store() -> Store {
    Store::connect(&db_url(), 5)
        .await
        .expect("connect + migrate")
}

/// A raw pool for reading columns the store API doesn't expose and for cleanup.
pub async fn raw_pool() -> PgPool {
    PgPoolOptions::new()
        .connect(&db_url())
        .await
        .expect("raw pool connect")
}

/// Delete an account and (via `ON DELETE CASCADE`) all its events, so repeated
/// test runs don't accumulate rows. Best-effort; call at the end of a test.
/// (A panic mid-test still leaves its row, but each test uses a unique random
/// user id, so the steady state stays clean.)
pub async fn cleanup_account(pool: &PgPool, account_id: Uuid) {
    let _ = sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
        .bind(account_id)
        .execute(pool)
        .await;
}

/// Removes a test trigger and its function on explicit cleanup or panic.
/// Register before creating either object so partial setup is covered too.
pub struct TriggerFixture {
    function: String,
    database_url: String,
    removed: bool,
}

impl TriggerFixture {
    pub fn new(function: &str) -> Self {
        assert!(function.len() <= 63);
        assert!(function
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        Self {
            function: function.to_owned(),
            database_url: db_url(),
            removed: false,
        }
    }

    pub async fn remove(&mut self, pool: &PgPool) {
        remove_trigger_function(pool, &self.function).await.unwrap();
        self.removed = true;
    }
}

async fn remove_trigger_function(pool: &PgPool, function: &str) -> Result<(), sqlx_core::Error> {
    // Each function belongs to one fixture; CASCADE also removes its trigger.
    sqlx_core::query::query(&format!("DROP FUNCTION IF EXISTS {function}() CASCADE"))
        .execute(pool)
        .await?;
    Ok(())
}

impl Drop for TriggerFixture {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        let database_url = self.database_url.clone();
        let function = self.function.clone();
        // Drop cannot await, and the test's runtime may already be unwinding.
        // A separate thread/runtime owns a fresh connection and completes cleanup
        // before the next test, bounded even if PostgreSQL is unavailable.
        let cleaned = std::thread::Builder::new()
            .name("sql-fixture-cleanup".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return false;
                };
                let cleaned = runtime.block_on(async {
                    tokio::time::timeout(std::time::Duration::from_secs(5), async {
                        let pool = PgPoolOptions::new()
                            .max_connections(1)
                            .connect(&database_url)
                            .await?;
                        let result = remove_trigger_function(&pool, &function).await;
                        pool.close().await;
                        result
                    })
                    .await
                    .is_ok_and(|result| result.is_ok())
                });
                runtime.shutdown_timeout(std::time::Duration::from_secs(1));
                cleaned
            })
            .is_ok_and(|thread| thread.join().unwrap_or(false));
        if !cleaned {
            // Connection errors may include credentials; keep diagnostics generic.
            eprintln!("SQL test fixture cleanup failed");
        }
    }
}
