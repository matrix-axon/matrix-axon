//! Integration tests for the per-device client-state KV (M12, ADR 0048).
//!
//! Like the other store tests these need a running Postgres and are `#[ignore]`d
//! by default. Run them with:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store -- --ignored
//! ```

mod common;

use axon_store::DeviceStateUpsert;
use common::test_account;
use serde_json::json;
use uuid::Uuid;

/// Upsert then read back: a device's write shows in the merged view, and a
/// second write to the same key overwrites in place with a newer `updated_at`.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn upsert_overwrites_in_place_and_bumps_updated_at() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "ds").await;
    let device = Uuid::new_v4();

    let v1 = json!({ "text": "first" });
    let t1 = store
        .upsert_device_state(&DeviceStateUpsert {
            account_id,
            device_id: device,
            namespace: "drafts",
            entries: &[("!room:localhost".to_owned(), Some(v1.clone()))],
        })
        .await
        .expect("write v1");

    let v2 = json!({ "text": "second" });
    let t2 = store
        .upsert_device_state(&DeviceStateUpsert {
            account_id,
            device_id: device,
            namespace: "drafts",
            entries: &[("!room:localhost".to_owned(), Some(v2.clone()))],
        })
        .await
        .expect("write v2");
    assert!(t2 > t1, "updated_at must advance on overwrite");

    let merged = store
        .merged_device_state(account_id, "drafts")
        .await
        .expect("merged read");
    assert_eq!(merged.len(), 1, "one key, one winner");
    assert_eq!(merged[0].key, "!room:localhost");
    assert_eq!(merged[0].device_id, device);
    assert_eq!(merged[0].value.as_ref().unwrap()["text"], "second");

    common::cleanup_account(&pool, account_id).await;
}

/// The merged view picks the newest write per key across two devices, and keys
/// are account- and namespace-scoped.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn merged_view_picks_newest_across_devices() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "ds").await;
    let other_account = test_account(&store, "ds-other").await;
    let device_a = Uuid::new_v4();
    let device_b = Uuid::new_v4();

    let from_a = json!({ "text": "from A" });
    store
        .upsert_device_state(&DeviceStateUpsert {
            account_id,
            device_id: device_a,
            namespace: "drafts",
            entries: &[("!shared:localhost".to_owned(), Some(from_a.clone()))],
        })
        .await
        .expect("A writes");

    // B writes the same key later — B wins the merge.
    let from_b = json!({ "text": "from B" });
    store
        .upsert_device_state(&DeviceStateUpsert {
            account_id,
            device_id: device_b,
            namespace: "drafts",
            entries: &[("!shared:localhost".to_owned(), Some(from_b.clone()))],
        })
        .await
        .expect("B writes");

    // A key only A holds still surfaces.
    let only_a = json!({ "text": "only A" });
    store
        .upsert_device_state(&DeviceStateUpsert {
            account_id,
            device_id: device_a,
            namespace: "drafts",
            entries: &[("!solo:localhost".to_owned(), Some(only_a.clone()))],
        })
        .await
        .expect("A solo write");

    // Same key under another namespace and another account: never merged in.
    let elsewhere = json!({ "x": 1 });
    store
        .upsert_device_state(&DeviceStateUpsert {
            account_id,
            device_id: device_a,
            namespace: "read_markers",
            entries: &[("!shared:localhost".to_owned(), Some(elsewhere.clone()))],
        })
        .await
        .expect("other namespace");
    store
        .upsert_device_state(&DeviceStateUpsert {
            account_id: other_account,
            device_id: device_a,
            namespace: "drafts",
            entries: &[("!shared:localhost".to_owned(), Some(elsewhere.clone()))],
        })
        .await
        .expect("other account");

    let merged = store
        .merged_device_state(account_id, "drafts")
        .await
        .expect("merged read");
    assert_eq!(merged.len(), 2);
    let shared = merged
        .iter()
        .find(|r| r.key == "!shared:localhost")
        .unwrap();
    assert_eq!(shared.device_id, device_b, "newest write wins");
    assert_eq!(shared.value.as_ref().unwrap()["text"], "from B");
    let solo = merged.iter().find(|r| r.key == "!solo:localhost").unwrap();
    assert_eq!(solo.device_id, device_a);

    common::cleanup_account(&pool, account_id).await;
    common::cleanup_account(&pool, other_account).await;
}

/// A tombstone (NULL value) that is the newest write hides the key from the
/// merged view — it must not resurrect another device's older value.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn newest_tombstone_hides_key_in_merged_view() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "ds").await;
    let device_a = Uuid::new_v4();
    let device_b = Uuid::new_v4();

    let draft = json!({ "text": "typed on A" });
    store
        .upsert_device_state(&DeviceStateUpsert {
            account_id,
            device_id: device_a,
            namespace: "drafts",
            entries: &[("!room:localhost".to_owned(), Some(draft.clone()))],
        })
        .await
        .expect("A writes");

    // B clears the draft (sent the message) — newest write is a tombstone.
    store
        .upsert_device_state(&DeviceStateUpsert {
            account_id,
            device_id: device_b,
            namespace: "drafts",
            entries: &[("!room:localhost".to_owned(), None)],
        })
        .await
        .expect("B tombstones");

    let merged = store
        .merged_device_state(account_id, "drafts")
        .await
        .expect("merged read");
    assert!(merged.is_empty(), "tombstone winner hides the key");

    // A writes again after the clear — the key resurfaces with the new value.
    let again = json!({ "text": "typed again" });
    store
        .upsert_device_state(&DeviceStateUpsert {
            account_id,
            device_id: device_a,
            namespace: "drafts",
            entries: &[("!room:localhost".to_owned(), Some(again.clone()))],
        })
        .await
        .expect("A rewrites");
    let merged = store
        .merged_device_state(account_id, "drafts")
        .await
        .expect("merged read");
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].value.as_ref().unwrap()["text"], "typed again");

    common::cleanup_account(&pool, account_id).await;
}

/// Deleting the account cascades its device state away.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn account_delete_cascades_device_state() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "ds").await;

    let v = json!({ "text": "doomed" });
    store
        .upsert_device_state(&DeviceStateUpsert {
            account_id,
            device_id: Uuid::new_v4(),
            namespace: "drafts",
            entries: &[("!room:localhost".to_owned(), Some(v.clone()))],
        })
        .await
        .expect("write");

    common::cleanup_account(&pool, account_id).await;

    use sqlx_core::row::Row;
    let row =
        sqlx_core::query::query("SELECT count(*) AS n FROM device_state WHERE account_id = $1")
            .bind(account_id)
            .fetch_one(&pool)
            .await
            .expect("count");
    let n: i64 = row.try_get("n").expect("n");
    assert_eq!(n, 0, "cascade removed the device_state rows");
}

/// A batch upsert is one atomic statement: mixed inserts, overwrites, and
/// tombstones land together and every row carries the same statement-stable
/// `updated_at` — the timestamp the API reports for the whole PUT.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn batch_upsert_is_single_statement_with_one_timestamp() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "ds").await;
    let device = Uuid::new_v4();

    // Seed one key so the batch mixes an UPDATE with INSERTs.
    let seed = json!({ "text": "old" });
    store
        .upsert_device_state(&DeviceStateUpsert {
            account_id,
            device_id: device,
            namespace: "drafts",
            entries: &[("!a:localhost".to_owned(), Some(seed.clone()))],
        })
        .await
        .expect("seed");

    let batch_ts = store
        .upsert_device_state(&DeviceStateUpsert {
            account_id,
            device_id: device,
            namespace: "drafts",
            entries: &[
                ("!a:localhost".to_owned(), Some(json!({ "text": "new" }))), // update
                ("!b:localhost".to_owned(), Some(json!({ "text": "b" }))),   // insert
                ("!c:localhost".to_owned(), None),                           // tombstone insert
            ],
        })
        .await
        .expect("batch");

    // Every row written by the batch shares the reported timestamp, and the
    // tombstone landed as SQL NULL (not JSON null).
    use sqlx_core::row::Row;
    let rows = sqlx_core::query::query(
        "SELECT key, value, value IS NULL AS tomb, updated_at FROM device_state \
         WHERE account_id = $1 AND namespace = 'drafts' ORDER BY key",
    )
    .bind(account_id)
    .fetch_all(&pool)
    .await
    .expect("rows");
    assert_eq!(rows.len(), 3);
    for row in &rows {
        let ts: chrono::DateTime<chrono::Utc> = row.try_get("updated_at").expect("ts");
        assert_eq!(ts, batch_ts, "statement-stable now() for every row");
    }
    let tomb: bool = rows[2].try_get("tomb").expect("tomb");
    assert!(tomb, "JSON null folded to SQL NULL");

    // The merged view reflects the batch: a updated, b present, c absent.
    let merged = store
        .merged_device_state(account_id, "drafts")
        .await
        .expect("merged");
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].value.as_ref().unwrap()["text"], "new");

    common::cleanup_account(&pool, account_id).await;
}

/// Write one row with a chosen `updated_at`, as a write whose statement started
/// then would carry. The `BEFORE UPDATE` trigger only fires on update, so an
/// insert keeps the timestamp it is given.
async fn insert_at(
    pool: &sqlx_postgres::PgPool,
    account_id: Uuid,
    device_id: Uuid,
    key: &str,
    value: Option<serde_json::Value>,
    updated_at: chrono::DateTime<chrono::Utc>,
) {
    sqlx_core::query::query(
        "INSERT INTO device_state (account_id, device_id, namespace, key, value, updated_at) \
         VALUES ($1, $2, 'read_markers', $3, $4, $5)",
    )
    .bind(account_id)
    .bind(device_id)
    .bind(key)
    .bind(value)
    .bind(updated_at)
    .execute(pool)
    .await
    .expect("insert backdated row");
}

/// A delta (ADR 0131) holds exactly the keys written since the cursor, each
/// with its current winner across devices, and a deleted key as a tombstone;
/// keys untouched since the cursor, and other namespaces, are left out.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn delta_returns_changed_keys_with_winners_and_tombstones() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "ds-delta").await;
    let device_a = Uuid::new_v4();
    let device_b = Uuid::new_v4();
    // Older than any overlap, so only what is written after the read counts.
    let long_ago = chrono::Utc::now() - chrono::Duration::hours(2);
    for key in ["!old:localhost", "!shared:localhost", "!gone:localhost"] {
        insert_at(
            &pool,
            account_id,
            device_a,
            key,
            Some(json!({ "by": "A" })),
            long_ago,
        )
        .await;
    }

    let first = store
        .read_device_state(account_id, "read_markers", None)
        .await
        .expect("full read");
    assert!(first.full, "a read without a cursor is full");
    assert_eq!(first.rows.len(), 3);

    store
        .upsert_device_state(&DeviceStateUpsert {
            account_id,
            device_id: device_b,
            namespace: "read_markers",
            entries: &[
                ("!shared:localhost".to_owned(), Some(json!({ "by": "B" }))),
                ("!gone:localhost".to_owned(), None),
            ],
        })
        .await
        .expect("B writes");
    store
        .upsert_device_state(&DeviceStateUpsert {
            account_id,
            device_id: device_b,
            namespace: "drafts",
            entries: &[("!old:localhost".to_owned(), Some(json!({ "text": "x" })))],
        })
        .await
        .expect("other namespace");

    let delta = store
        .read_device_state(account_id, "read_markers", Some(first.as_of))
        .await
        .expect("delta read");
    assert!(!delta.full, "a cursor from this clock gets a delta");
    assert!(
        delta.as_of >= first.as_of,
        "the cursor never moves backwards"
    );
    let keys: Vec<&str> = delta.rows.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(
        keys,
        ["!gone:localhost", "!shared:localhost"],
        "only the written keys, by key"
    );
    assert_eq!(delta.rows[0].device_id, device_b);
    assert!(
        delta.rows[0].value.is_none(),
        "a deleted key is a tombstone in a delta"
    );
    assert_eq!(delta.rows[1].device_id, device_b, "the newest write wins");
    assert_eq!(delta.rows[1].value.as_ref().unwrap()["by"], "B");

    // The full read still hides the tombstone.
    let full = store
        .read_device_state(account_id, "read_markers", None)
        .await
        .expect("full read again");
    let keys: Vec<&str> = full.rows.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(keys, ["!old:localhost", "!shared:localhost"]);

    common::cleanup_account(&pool, account_id).await;
}

/// A write whose statement started before a read but committed after it has
/// an `updated_at` older than the read's cursor. The delta reaches back by the
/// overlap to catch it, and no further (ADR 0131).
#[tokio::test]
#[ignore = "requires Postgres"]
async fn delta_reaches_back_by_the_overlap() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "ds-overlap").await;
    let device = Uuid::new_v4();
    let overlap = store.device_state_overlap();
    assert_eq!(
        overlap,
        chrono::Duration::minutes(5),
        "default statement timeout → floor"
    );

    let read = store
        .read_device_state(account_id, "read_markers", None)
        .await
        .expect("full read");
    assert!(read.rows.is_empty());

    // Both land after the read; one started inside the overlap, one before it.
    let late = read.as_of - overlap + chrono::Duration::seconds(5);
    let too_late = read.as_of - overlap - chrono::Duration::seconds(5);
    insert_at(
        &pool,
        account_id,
        device,
        "!late:localhost",
        Some(json!(1)),
        late,
    )
    .await;
    insert_at(
        &pool,
        account_id,
        device,
        "!ancient:localhost",
        Some(json!(2)),
        too_late,
    )
    .await;

    let delta = store
        .read_device_state(account_id, "read_markers", Some(read.as_of))
        .await
        .expect("delta read");
    assert!(!delta.full);
    let keys: Vec<&str> = delta.rows.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(keys, ["!late:localhost"], "inside the overlap only");

    common::cleanup_account(&pool, account_id).await;
}

/// A cursor later than the database clock did not come from it, so the read is
/// served in full instead of as a delta that could miss writes (ADR 0131).
#[tokio::test]
#[ignore = "requires Postgres"]
async fn cursor_from_the_future_reads_in_full() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "ds-future").await;
    let device = Uuid::new_v4();
    let long_ago = chrono::Utc::now() - chrono::Duration::hours(2);
    insert_at(
        &pool,
        account_id,
        device,
        "!kept:localhost",
        Some(json!(1)),
        long_ago,
    )
    .await;
    insert_at(
        &pool,
        account_id,
        device,
        "!deleted:localhost",
        None,
        long_ago,
    )
    .await;

    let future = chrono::Utc::now() + chrono::Duration::hours(1);
    let read = store
        .read_device_state(account_id, "read_markers", Some(future))
        .await
        .expect("read");
    assert!(read.full, "a future cursor is answered in full");
    let keys: Vec<&str> = read.rows.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(keys, ["!kept:localhost"], "a full read omits tombstones");
    assert!(read.as_of < future);

    common::cleanup_account(&pool, account_id).await;
}
