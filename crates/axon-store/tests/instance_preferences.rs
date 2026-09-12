//! Integration tests for instance-wide preferences (ADR 0103).
//!
//! Like the other store tests these need a running Postgres and are `#[ignore]`d
//! by default. Run them with:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store -- --ignored
//! ```

mod common;

use serde_json::json;

/// A key that has never been written reads as `None`; a PUT overwrites in
/// place and bumps `updated_at`.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn instance_preference_put_get_overwrite() {
    let store = common::migrated_store().await;
    let key = format!("space_order-test-{}", uuid::Uuid::new_v4());

    assert!(store
        .instance_preference(&key)
        .await
        .expect("read missing")
        .is_none());

    let v1 = json!({ "spaces": ["aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa/!a:localhost"] });
    let t1 = store
        .upsert_instance_preference(&key, &v1)
        .await
        .expect("write v1");
    let row = store
        .instance_preference(&key)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(row.key, key);
    assert_eq!(row.value, v1);
    assert_eq!(row.updated_at, t1);

    let v2 = json!({ "spaces": [] });
    let t2 = store
        .upsert_instance_preference(&key, &v2)
        .await
        .expect("write v2");
    assert!(t2 > t1, "updated_at must advance on overwrite");
    let row = store
        .instance_preference(&key)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(row.value, v2);
    assert_eq!(row.updated_at, t2);

    sqlx_core::query::query("DELETE FROM instance_preferences WHERE key = $1")
        .bind(&key)
        .execute(store.pool())
        .await
        .expect("cleanup");
}
