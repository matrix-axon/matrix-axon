//! DB-gated tests for the ADR 0140 gap rows: the first-window check, the
//! token-rejection restart, lag probes, and the purge and teardown sites
//! (decision 6).
//!
//! Require a running Postgres and are `#[ignore]`d by default. Run with:
//!
//! ```sh
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store --test timeline_gaps -- --ignored
//! ```

mod common;

use std::time::Duration;

use axon_store::{GapCheck, GapOrigin, NewEvent, RoomBackfillFailure, Store, TimelineGap};
use common::{insert_message, migrated_store, raw_pool, test_account};
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(600);

fn room() -> String {
    format!("!gaps-{}:localhost", Uuid::new_v4())
}

async fn gap(store: &Store, account_id: Uuid, id: i64) -> Option<TimelineGap> {
    store
        .list_timeline_gaps(account_id)
        .await
        .expect("list gaps")
        .into_iter()
        .find(|gap| gap.id == id)
}

/// Each insert is its own transaction, so `received_at` strictly increases;
/// the short sleep keeps that true on a coarse database clock.
async fn store_events(store: &Store, account_id: Uuid, room_id: &str, n: usize) -> Vec<String> {
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        ids.push(insert_message(store, account_id, room_id, i as i64, "m").await);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    ids
}

/// The first-window test: a batch with nothing stored before it is the room's
/// first window, whatever was stored after it (later responses, backfill).
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_batch_with_nothing_stored_before_it_is_a_first_window() {
    let store = migrated_store().await;
    let account_id = test_account(&store, "gaps-first").await;
    let room_id = room();
    let batch = store_events(&store, account_id, &room_id, 3).await;
    // Stored after the batch: a later response's events and backfilled ones.
    store_events(&store, account_id, &room_id, 3).await;

    let id = store
        .record_limited_gap(account_id, &room_id, Some("t-prev"), &batch)
        .await
        .expect("record");
    assert_eq!(
        store.check_timeline_gap(id, WAIT).await.expect("check"),
        GapCheck::FirstWindow
    );
    assert!(gap(&store, account_id, id).await.is_none(), "dropped");
    store.delete_account_row(account_id).await.expect("cleanup");
}

/// A batch with an older stored event beside it is a hole. Without a
/// `prev_batch`, its boundary resolves to the batch's earliest storage time.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_batch_after_stored_history_is_a_hole_with_a_resolved_boundary() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "gaps-hole").await;
    let room_id = room();
    store_events(&store, account_id, &room_id, 2).await;
    let batch = store_events(&store, account_id, &room_id, 3).await;

    let id = store
        .record_limited_gap(account_id, &room_id, None, &batch)
        .await
        .expect("record");
    let before = gap(&store, account_id, id).await.expect("row");
    assert_eq!(before.stored_before, None, "no prev_batch: resolved later");
    assert!(!before.batch_checked);
    assert_eq!(
        store.check_timeline_gap(id, WAIT).await.expect("check"),
        GapCheck::Hole
    );
    let after = gap(&store, account_id, id).await.expect("row kept");
    assert!(after.batch_checked);
    let (batch_min,): (chrono::DateTime<chrono::Utc>,) = sqlx_core::query_as::query_as(
        "SELECT min(received_at) FROM events WHERE account_id = $1 AND event_id = ANY($2)",
    )
    .bind(account_id)
    .bind(&batch)
    .fetch_one(&pool)
    .await
    .expect("batch min");
    assert_eq!(after.stored_before, Some(batch_min));
    assert_eq!(after.from_token, None, "pages from the live end");
    store.delete_account_row(account_id).await.expect("cleanup");
}

/// A row with a `prev_batch` resolves its boundary at the check too, to the
/// batch's earliest storage time, so recording it before or after its batch is
/// stored makes no difference.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_limited_row_with_prev_batch_resolves_its_boundary_from_the_batch() {
    let store = migrated_store().await;
    let account_id = test_account(&store, "gaps-prev").await;
    let room_id = room();
    store_events(&store, account_id, &room_id, 2).await;
    // Recorded before its batch is stored, as the ingestion handler does.
    let batch_ids: Vec<String> = (0..3)
        .map(|_| format!("$batch-{}:localhost", Uuid::new_v4()))
        .collect();
    let id = store
        .record_limited_gap(account_id, &room_id, Some("t-prev"), &batch_ids)
        .await
        .expect("record");
    let row = gap(&store, account_id, id).await.expect("row");
    assert_eq!(row.origin, GapOrigin::Limited);
    assert_eq!(row.from_token.as_deref(), Some("t-prev"));
    assert_eq!(row.stored_before, None, "resolved at the check");
    assert_eq!(
        store.check_timeline_gap(id, WAIT).await.expect("check"),
        GapCheck::AwaitingBatch
    );
    store
        .upsert_event(&NewEvent {
            event_id: &batch_ids[0],
            room_id: &room_id,
            account_id,
            sender: "@alice:localhost",
            origin_ts: 10,
            event_type: "m.room.message",
            content: None,
            raw_event: serde_json::json!({ "type": "m.room.message" }),
            megolm_session_id: None,
            redacts: None,
            relates_to: None,
            decrypted_body_text: None,
        })
        .await
        .expect("store the batch's first event");
    assert_eq!(
        store.check_timeline_gap(id, WAIT).await.expect("check"),
        GapCheck::Hole
    );
    let batch_min = store
        .lag_mark(account_id, &batch_ids)
        .await
        .expect("mark")
        .expect("stored")
        .received_at;
    let row = gap(&store, account_id, id).await.expect("row");
    assert_eq!(row.stored_before, Some(batch_min));
    assert_eq!(
        row.from_token.as_deref(),
        Some("t-prev"),
        "pages from prev_batch"
    );
    store.delete_account_row(account_id).await.expect("cleanup");
}

/// A row waits for its batch, and is dropped once the wait limit has passed
/// with none of it stored.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_row_awaits_its_batch_then_expires() {
    let store = migrated_store().await;
    let account_id = test_account(&store, "gaps-await").await;
    let room_id = room();
    let id = store
        .record_limited_gap(account_id, &room_id, Some("t-1"), &["$never".to_owned()])
        .await
        .expect("record");
    assert_eq!(
        store.check_timeline_gap(id, WAIT).await.expect("check"),
        GapCheck::AwaitingBatch
    );
    assert!(
        gap(&store, account_id, id).await.is_some(),
        "kept, not dropped"
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        store
            .check_timeline_gap(id, Duration::from_millis(1))
            .await
            .expect("check"),
        GapCheck::Expired {
            from_token: Some("t-1".to_owned())
        }
    );
    assert!(gap(&store, account_id, id).await.is_none());
    store.delete_account_row(account_id).await.expect("cleanup");
}

/// A rejected token restarts the row from the live end, bounded by the batch's
/// earliest storage time; a lag probe keeps its boundary.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_rejected_token_restarts_from_the_live_end() {
    let store = migrated_store().await;
    let account_id = test_account(&store, "gaps-reject").await;
    let room_id = room();
    store_events(&store, account_id, &room_id, 2).await;
    let batch = store_events(&store, account_id, &room_id, 2).await;
    let id = store
        .record_limited_gap(account_id, &room_id, Some("t-old"), &batch)
        .await
        .expect("record");
    assert_eq!(
        store.check_timeline_gap(id, WAIT).await.expect("check"),
        GapCheck::Hole
    );
    store
        .save_timeline_gap_page(id, "t-deeper", 4)
        .await
        .expect("page");
    let rejected = RoomBackfillFailure {
        kind: "rejected",
        retry_after: Duration::ZERO,
        parked: false,
        clear_token: true,
        timed_out: false,
    };
    store
        .record_timeline_gap_failure(id, rejected)
        .await
        .expect("failure");
    let row = gap(&store, account_id, id).await.expect("row");
    assert_eq!(row.origin, GapOrigin::TokenRejected);
    assert_eq!(row.from_token, None);
    assert_eq!(row.consecutive_failures, 1);
    let batch_min = store
        .lag_mark(account_id, &batch[..1])
        .await
        .expect("first batch event")
        .expect("stored")
        .received_at;
    assert_eq!(row.stored_before, Some(batch_min));
    assert_eq!(row.events_recovered, 4, "progress is kept");

    // A probe has no batch, so it keeps its boundary.
    let mark = store.lag_mark_now().await.expect("now");
    let boundary = mark.received_at;
    store_events(&store, account_id, &room_id, 1).await;
    let probes = store
        .record_lag_probes(account_id, mark, std::slice::from_ref(&room_id))
        .await
        .expect("probe");
    assert_eq!(probes, vec![room_id.clone()]);
    let probe = store
        .list_timeline_gaps(account_id)
        .await
        .expect("list")
        .into_iter()
        .find(|gap| gap.origin == GapOrigin::LagProbe)
        .expect("probe row");
    store
        .record_timeline_gap_failure(probe.id, rejected)
        .await
        .expect("failure");
    let probe = gap(&store, account_id, probe.id).await.expect("row");
    assert_eq!(probe.stored_before, Some(boundary));
    store.delete_account_row(account_id).await.expect("cleanup");
}

/// Probes go to joined rooms that stored an event at or after the boundary,
/// and carry each room's lowest-id event as their first-window stop.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn lag_probes_cover_joined_rooms_active_since_the_boundary() {
    let store = migrated_store().await;
    let account_id = test_account(&store, "gaps-probe").await;
    let (quiet, busy, left) = (room(), room(), room());
    store_events(&store, account_id, &quiet, 2).await;
    let busy_first = store_events(&store, account_id, &busy, 3).await;
    let mark = store.lag_mark_now().await.expect("now");
    let boundary = mark.received_at;
    store_events(&store, account_id, &busy, 2).await;
    store_events(&store, account_id, &left, 2).await;

    let joined = vec![quiet.clone(), busy.clone()];
    let probes = store
        .record_lag_probes(account_id, mark, &joined)
        .await
        .expect("probe");
    assert_eq!(
        probes,
        vec![busy.clone()],
        "quiet and left rooms are not probed"
    );
    let probe = store
        .list_timeline_gaps(account_id)
        .await
        .expect("list")
        .pop()
        .expect("row");
    assert_eq!(probe.origin, GapOrigin::LagProbe);
    assert!(probe.batch_checked, "probes have no batch to wait for");
    assert_eq!(probe.from_token, None);
    assert_eq!(probe.stored_before, Some(boundary));
    assert_eq!(
        probe.stop_event_ids,
        busy_first[..1].to_vec(),
        "the first event stored in the room"
    );

    // The stop rule: an event stored before the boundary, or a stop event.
    let lookup = store
        .lookup_gap_page(account_id, &probe, &busy_first[2..])
        .await
        .expect("lookup");
    assert_eq!(lookup.already_stored, 1);
    assert!(lookup.reaches_stored_history, "stored before the boundary");
    store.delete_account_row(account_id).await.expect("cleanup");
}

/// Decision 6: a purge deletes the room's gap rows under the preserve guard,
/// and account deletion cascades.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn purge_and_account_deletion_remove_gap_rows() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "gaps-purge").await;
    let (purged, kept) = (room(), room());
    store_events(&store, account_id, &purged, 1).await;
    store
        .record_limited_gap(account_id, &purged, Some("t"), &[])
        .await
        .expect("record");
    store
        .record_limited_gap(account_id, &kept, Some("t"), &[])
        .await
        .expect("record");
    store.purge_room(account_id, &purged).await.expect("purge");
    let rooms: Vec<String> = store
        .list_timeline_gaps(account_id)
        .await
        .expect("list")
        .into_iter()
        .map(|gap| gap.room_id)
        .collect();
    assert_eq!(rooms, vec![kept.clone()], "only the purged room's rows go");

    // A room rejoined before its purge runs keeps rows recorded after the
    // rejoin: events above the purge watermark preserve the room.
    let rejoined = room();
    store_events(&store, account_id, &rejoined, 1).await;
    store
        .queue_room_purge(account_id, &rejoined)
        .await
        .expect("queue");
    store_events(&store, account_id, &rejoined, 1).await;
    let after_rejoin = store
        .record_limited_gap(account_id, &rejoined, Some("t"), &[])
        .await
        .expect("record");
    store
        .purge_room(account_id, &rejoined)
        .await
        .expect("purge");
    assert!(gap(&store, account_id, after_rejoin).await.is_some());

    store.delete_account_row(account_id).await.expect("delete");
    let (left,): (i64,) = sqlx_core::query_as::query_as(
        "SELECT count(*) FROM room_timeline_gaps WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(left, 0, "rows cascade with the account");
}

/// Review finding (#1143): a gap page counts what it recovered from its own
/// writes, so an upsert reports whether it created the row. And an event
/// Postgres can never accept is a permanent refusal, which a pager skips
/// rather than retrying the page for good.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn an_upsert_reports_creation_and_a_nul_escape_is_permanent() {
    let store = migrated_store().await;
    let account_id = test_account(&store, "gaps-upsert").await;
    let room_id = room();
    let event = |event_id: &'static str, body: &'static str| NewEvent {
        event_id,
        room_id: &room_id,
        account_id,
        sender: "@alice:localhost",
        origin_ts: 10,
        event_type: "m.room.message",
        content: Some(serde_json::json!({ "body": body })),
        raw_event: serde_json::json!({ "type": "m.room.message", "content": { "body": body } }),
        megolm_session_id: None,
        redacts: None,
        relates_to: None,
        decrypted_body_text: None,
    };

    let first = store
        .upsert_event_outcome(&event("$created", "hi"))
        .await
        .expect("insert");
    assert!(first.created);
    let again = store
        .upsert_event_outcome(&event("$created", "hi"))
        .await
        .expect("re-deliver");
    assert!(!again.created, "already stored");
    assert_eq!(again.arrival_order, first.arrival_order);

    // jsonb cannot hold U+0000 (SQLSTATE 22P05), however often it is tried.
    let err = store
        .upsert_event_outcome(&event("$nul", "a\u{0}b"))
        .await
        .expect_err("jsonb rejects a NUL");
    assert!(!err.is_transient(), "{err}");
    store.delete_account_row(account_id).await.expect("cleanup");
}
