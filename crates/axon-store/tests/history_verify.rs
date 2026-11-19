//! DB-gated tests for the ADR 0140 verification rows (decision 4, as amended
//! 2026-11-18): queueing, the stop points a walk starts with, page lookups,
//! failures, the purge and teardown sites (decision 6), and the expired gap
//! row that re-queues its room (#1170).
//!
//! Require a running Postgres and are `#[ignore]`d by default. Run with:
//!
//! ```sh
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store --test history_verify -- --ignored
//! ```

mod common;

use std::time::Duration;

use axon_store::{
    GapCheck, HistoryVerifyRow, RoomBackfillFailure, Store, VerifyStop, WalkProgress, WalkStart,
};
use common::{insert_message, migrated_store, raw_pool, test_account};
use uuid::Uuid;

fn room() -> String {
    format!("!verify-{}:localhost", Uuid::new_v4())
}

async fn row(store: &Store, account_id: Uuid, room_id: &str) -> Option<HistoryVerifyRow> {
    store
        .list_history_verify(account_id)
        .await
        .expect("list rows")
        .into_iter()
        .find(|row| row.room_id == room_id)
}

/// The walk's current generation, which every write names.
async fn generation(store: &Store, account_id: Uuid, room_id: &str) -> i64 {
    store
        .history_verify_guard(account_id, room_id)
        .await
        .expect("guard")
        .expect("row exists")
        .walk_generation
}

/// A page's progress, with events (so it clears failures) unless `empty`.
fn progress(end: &str, inserted: i64, floor_run: i32, empty_run: i32) -> WalkProgress<'_> {
    WalkProgress {
        end,
        inserted,
        floor_run,
        empty_run,
        clears_failures: empty_run == 0,
    }
}

/// Store one message per timestamp, in order, so the first has the lowest id.
async fn store_events(store: &Store, account_id: Uuid, room_id: &str, ts: &[i64]) -> Vec<String> {
    let mut ids = Vec::with_capacity(ts.len());
    for origin_ts in ts {
        ids.push(insert_message(store, account_id, room_id, *origin_ts, "m").await);
    }
    ids
}

/// The row's `complete` and `stop_reason`, which the list (incomplete rows
/// only) does not show once a walk closes.
async fn completion(
    pool: &sqlx_postgres::PgPool,
    account_id: Uuid,
    room_id: &str,
) -> Option<(bool, Option<String>, i64)> {
    sqlx_core::query_as::query_as(
        "SELECT complete, stop_reason, events_recovered FROM room_history_verify \
         WHERE account_id = $1 AND room_id = $2",
    )
    .bind(account_id)
    .bind(room_id)
    .fetch_optional(pool)
    .await
    .expect("read row")
}

/// The management action and the seed queue every room with stored events,
/// and no room without; re-queueing a finished walk restarts it with no stop
/// points.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn requeue_queues_rooms_with_stored_events_and_restarts_their_walks() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "verify-requeue").await;
    let (a, b, empty) = (room(), room(), room());
    store_events(&store, account_id, &a, &[1, 2]).await;
    store_events(&store, account_id, &b, &[3]).await;

    assert_eq!(
        store
            .requeue_history_verify(account_id, None)
            .await
            .expect("queue all"),
        2
    );
    assert!(row(&store, account_id, &a).await.is_some());
    assert!(row(&store, account_id, &b).await.is_some());
    assert_eq!(
        store
            .requeue_history_verify(account_id, Some(&empty))
            .await
            .expect("queue empty"),
        0,
        "a room with no stored event has nothing to verify"
    );

    let g = generation(&store, account_id, &a).await;
    store
        .start_history_verify(account_id, &a, g, 20)
        .await
        .expect("start");
    store
        .complete_history_verify(account_id, &a, g, VerifyStop::FirstStoredEvent, true, 4)
        .await
        .expect("complete")
        .expect("same generation");
    assert!(
        row(&store, account_id, &a).await.is_none(),
        "complete rows are not listed"
    );
    assert_eq!(
        completion(&pool, account_id, &a).await,
        Some((true, Some("first_stored_event".to_owned()), 4))
    );

    assert_eq!(
        store
            .requeue_history_verify(account_id, Some(&a))
            .await
            .expect("queue one"),
        1
    );
    let requeued = row(&store, account_id, &a).await.expect("listed again");
    assert_eq!(requeued.stop_event_ids, None, "fresh stop points");
    assert_eq!(requeued.from_token, None, "from the live end");
    assert_eq!(requeued.events_recovered, 0);
    store.delete_account_row(account_id).await.expect("cleanup");
}

/// A walk stops at the room's single lowest-id event, and its floor is the
/// oldest `origin_ts` among the first `timeline_limit` events, not the stop
/// event's own, so one event with a wrong clock cannot raise it.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_walk_starts_with_the_lowest_id_event_and_the_oldest_of_the_first_events() {
    let store = migrated_store().await;
    let account_id = test_account(&store, "verify-start").await;
    let room_id = room();
    let ids = store_events(&store, account_id, &room_id, &[100, 50, 70, 10]).await;
    store
        .requeue_history_verify(account_id, Some(&room_id))
        .await
        .expect("queue");

    let g = generation(&store, account_id, &room_id).await;
    let WalkStart::Started(stops) = store
        .start_history_verify(account_id, &room_id, g, 3)
        .await
        .expect("start")
    else {
        panic!("the room has stored events");
    };
    assert_eq!(stops.stop_event_ids, vec![ids[0].clone()]);
    assert_eq!(stops.floor_ts, Some(50), "the oldest of the first three");
    let listed = row(&store, account_id, &room_id).await.expect("listed");
    assert_eq!(listed.stop_event_ids, Some(vec![ids[0].clone()]));
    assert_eq!(listed.floor_ts, Some(50));

    // A purged room's row goes with its history.
    let purged = room();
    store_events(&store, account_id, &purged, &[1]).await;
    store
        .requeue_history_verify(account_id, Some(&purged))
        .await
        .expect("queue");
    let g = generation(&store, account_id, &purged).await;
    store.purge_room(account_id, &purged).await.expect("purge");
    assert_eq!(
        store
            .start_history_verify(account_id, &purged, g, 3)
            .await
            .expect("start"),
        WalkStart::Stale,
        "the purge deleted the row with the history"
    );
    store.delete_account_row(account_id).await.expect("cleanup");
}

/// The page lookup runs before the page is written: it reports the events
/// already stored and whether the stop event is among them.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_page_lookup_reports_what_was_stored_and_the_stop() {
    let store = migrated_store().await;
    let account_id = test_account(&store, "verify-lookup").await;
    let room_id = room();
    let ids = store_events(&store, account_id, &room_id, &[1, 2]).await;
    let page = vec![ids[1].clone(), "$missing:localhost".to_owned()];

    let lookup = store
        .lookup_verify_page(account_id, &page, std::slice::from_ref(&ids[0]))
        .await
        .expect("lookup");
    assert_eq!(lookup.stored.len(), 1);
    assert!(lookup.stored.contains(&ids[1]));
    assert!(!lookup.reaches_stop);

    let page = vec![ids[0].clone()];
    let lookup = store
        .lookup_verify_page(account_id, &page, std::slice::from_ref(&ids[0]))
        .await
        .expect("lookup");
    assert!(lookup.reaches_stop);
    store.delete_account_row(account_id).await.expect("cleanup");
}

/// Progress is saved per page; a rejected token restarts the walk from the
/// live end with its stop points and counts the rejection, and any other
/// failure keeps the token and backs off (ADR 0121).
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_rejected_token_restarts_the_walk_from_the_live_end() {
    let store = migrated_store().await;
    let account_id = test_account(&store, "verify-reject").await;
    let room_id = room();
    store_events(&store, account_id, &room_id, &[1]).await;
    store
        .requeue_history_verify(account_id, Some(&room_id))
        .await
        .expect("queue");
    let g = generation(&store, account_id, &room_id).await;
    store
        .start_history_verify(account_id, &room_id, g, 20)
        .await
        .expect("start");
    assert!(store
        .save_history_verify_page(account_id, &room_id, g, progress("t-1", 3, 40, 0))
        .await
        .expect("save"));
    let saved = row(&store, account_id, &room_id).await.expect("listed");
    assert_eq!(saved.from_token.as_deref(), Some("t-1"));
    assert_eq!(
        (saved.pages_fetched, saved.events_recovered, saved.floor_run),
        (1, 3, 40)
    );

    let retryable = RoomBackfillFailure {
        kind: "retryable",
        retry_after: Duration::from_secs(60),
        parked: false,
        clear_token: false,
        timed_out: false,
    };
    store
        .record_history_verify_failure(account_id, &room_id, g, retryable)
        .await
        .expect("fail");
    let failed = row(&store, account_id, &room_id).await.expect("listed");
    assert_eq!(failed.from_token.as_deref(), Some("t-1"), "same token");
    assert!(!failed.retry_due, "backing off");
    assert_eq!(failed.consecutive_failures, 1);

    let rejected = RoomBackfillFailure {
        kind: "rejected",
        retry_after: Duration::ZERO,
        parked: false,
        clear_token: true,
        timed_out: false,
    };
    store
        .record_history_verify_failure(account_id, &room_id, g, rejected)
        .await
        .expect("reject");
    let restarted = row(&store, account_id, &room_id).await.expect("listed");
    assert_eq!(restarted.from_token, None, "back to the live end");
    assert_eq!(restarted.floor_run, 0);
    assert_eq!(restarted.token_rejections, 1);
    assert!(restarted.stop_event_ids.is_some(), "stop points kept");
    store.delete_account_row(account_id).await.expect("cleanup");
}

/// A room with an open gap row is held, and the joined flag is recorded for
/// status.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_room_with_an_open_gap_row_is_held() {
    let store = migrated_store().await;
    let account_id = test_account(&store, "verify-held").await;
    let room_id = room();
    store_events(&store, account_id, &room_id, &[1]).await;
    store
        .requeue_history_verify(account_id, Some(&room_id))
        .await
        .expect("queue");
    assert!(!row(&store, account_id, &room_id).await.expect("row").held);

    let gap = store
        .record_limited_gap(account_id, &room_id, Some("t"), &[])
        .await
        .expect("record");
    assert!(row(&store, account_id, &room_id).await.expect("row").held);
    store.delete_timeline_gap(gap).await.expect("close");
    assert!(!row(&store, account_id, &room_id).await.expect("row").held);

    store
        .mark_history_verify_joined(account_id, std::slice::from_ref(&room_id), false)
        .await
        .expect("mark");
    assert!(
        row(&store, account_id, &room_id)
            .await
            .expect("row")
            .not_joined
    );
    store
        .mark_history_verify_joined(account_id, std::slice::from_ref(&room_id), true)
        .await
        .expect("mark");
    assert!(
        !row(&store, account_id, &room_id)
            .await
            .expect("row")
            .not_joined
    );
    store.delete_account_row(account_id).await.expect("cleanup");
}

/// #1170: a limited row whose batch was never stored is dropped unpaged, and
/// the same statement queues the room's walk again, so the hole is found from
/// the live end instead of being lost. A finished walk restarts too.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn an_expired_gap_row_requeues_the_rooms_verification() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "verify-expired").await;
    let room_id = room();
    store_events(&store, account_id, &room_id, &[1]).await;
    store
        .requeue_history_verify(account_id, Some(&room_id))
        .await
        .expect("queue");
    let g = generation(&store, account_id, &room_id).await;
    store
        .complete_history_verify(
            account_id,
            &room_id,
            g,
            VerifyStop::FirstStoredEvent,
            true,
            0,
        )
        .await
        .expect("complete");

    let id = store
        .record_limited_gap(account_id, &room_id, Some("t-1"), &["$never".to_owned()])
        .await
        .expect("record");
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
    let requeued = row(&store, account_id, &room_id)
        .await
        .expect("the room's walk is pending again");
    assert_eq!(requeued.stop_event_ids, None);
    assert!(!requeued.held, "the gap row is gone");
    assert_eq!(
        completion(&pool, account_id, &room_id).await.map(|c| c.0),
        Some(false)
    );
    store.delete_account_row(account_id).await.expect("cleanup");
}

/// Decision 6: a purge deletes the room's verification row under the
/// preserve guard; a kept room's incomplete walk restarts with fresh stop
/// points, and its finished walk stays finished. Account deletion cascades.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn purge_and_account_deletion_remove_verification_rows() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "verify-purge").await;
    let (purged, untouched) = (room(), room());
    store_events(&store, account_id, &purged, &[1]).await;
    store_events(&store, account_id, &untouched, &[1]).await;
    store
        .requeue_history_verify(account_id, None)
        .await
        .expect("queue");
    store.purge_room(account_id, &purged).await.expect("purge");
    assert!(
        completion(&pool, account_id, &purged).await.is_none(),
        "deleted"
    );
    assert!(row(&store, account_id, &untouched).await.is_some());

    // Rejoined before the purge ran: the room is kept, and its walk, which
    // may be heading for an event the purge deleted, starts over.
    let (walking, finished) = (room(), room());
    for room_id in [&walking, &finished] {
        store_events(&store, account_id, room_id, &[1]).await;
        store
            .requeue_history_verify(account_id, Some(room_id))
            .await
            .expect("queue");
        let g = generation(&store, account_id, room_id).await;
        store
            .start_history_verify(account_id, room_id, g, 20)
            .await
            .expect("start");
        store
            .queue_room_purge(account_id, room_id)
            .await
            .expect("queue purge");
        store_events(&store, account_id, room_id, &[2]).await;
    }
    let g = generation(&store, account_id, &walking).await;
    store
        .save_history_verify_page(account_id, &walking, g, progress("t-deep", 0, 7, 0))
        .await
        .expect("save");
    let g = generation(&store, account_id, &finished).await;
    store
        .complete_history_verify(
            account_id,
            &finished,
            g,
            VerifyStop::FirstStoredEvent,
            true,
            0,
        )
        .await
        .expect("complete");
    store.purge_room(account_id, &walking).await.expect("purge");
    store
        .purge_room(account_id, &finished)
        .await
        .expect("purge");
    let restarted = row(&store, account_id, &walking).await.expect("kept");
    assert_eq!(restarted.from_token, None);
    assert_eq!(restarted.stop_event_ids, None, "fresh stop points");
    assert_eq!(restarted.floor_run, 0);
    assert_eq!(
        completion(&pool, account_id, &finished).await.map(|c| c.0),
        Some(true),
        "a finished walk stays finished"
    );

    store.delete_account_row(account_id).await.expect("delete");
    let (left,): (i64,) = sqlx_core::query_as::query_as(
        "SELECT count(*) FROM room_history_verify WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(left, 0, "rows cascade with the account");
}

/// Review finding: a restart (the management action, an expired gap row, a
/// kept room's purge) bumps the walk's generation, and every write a walk
/// makes names the generation it read. A walk read before the restart cannot
/// write its deep token back or close the restarted walk.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_restart_refuses_every_write_of_the_walk_it_replaced() {
    let store = migrated_store().await;
    let account_id = test_account(&store, "verify-fence").await;
    let room_id = room();
    store_events(&store, account_id, &room_id, &[1, 2]).await;
    store
        .requeue_history_verify(account_id, Some(&room_id))
        .await
        .expect("queue");
    let stale = generation(&store, account_id, &room_id).await;
    store
        .start_history_verify(account_id, &room_id, stale, 20)
        .await
        .expect("start");
    store
        .requeue_history_verify(account_id, Some(&room_id))
        .await
        .expect("restart");
    assert_eq!(generation(&store, account_id, &room_id).await, stale + 1);

    assert!(!store
        .save_history_verify_page(account_id, &room_id, stale, progress("t-deep", 5, 0, 0))
        .await
        .expect("save"));
    assert!(store
        .complete_history_verify(
            account_id,
            &room_id,
            stale,
            VerifyStop::FirstStoredEvent,
            true,
            0
        )
        .await
        .expect("complete")
        .is_none());
    let failure = RoomBackfillFailure {
        kind: "retryable",
        retry_after: Duration::from_secs(60),
        parked: false,
        clear_token: false,
        timed_out: false,
    };
    assert!(!store
        .record_history_verify_failure(account_id, &room_id, stale, failure)
        .await
        .expect("fail"));
    assert_eq!(
        store
            .start_history_verify(account_id, &room_id, stale, 20)
            .await
            .expect("start"),
        WalkStart::Stale
    );
    let restarted = row(&store, account_id, &room_id).await.expect("listed");
    assert_eq!(
        restarted.from_token, None,
        "the deep token was not written back"
    );
    assert_eq!(restarted.stop_event_ids, None);
    assert_eq!(restarted.consecutive_failures, 0);
    assert!(restarted.retry_due);
    store.delete_account_row(account_id).await.expect("cleanup");
}

/// Review finding: a room with no stored event left closes as `no_history` in
/// the statement that reads its stop points, so no crash can leave a started
/// walk with nothing to stop at.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_room_with_no_history_left_closes_as_it_starts() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "verify-nohistory").await;
    let room_id = room();
    store_events(&store, account_id, &room_id, &[1]).await;
    store
        .requeue_history_verify(account_id, Some(&room_id))
        .await
        .expect("queue");
    sqlx_core::query::query("DELETE FROM events WHERE account_id = $1 AND room_id = $2")
        .bind(account_id)
        .bind(&room_id)
        .execute(&pool)
        .await
        .expect("delete");
    let g = generation(&store, account_id, &room_id).await;
    assert_eq!(
        store
            .start_history_verify(account_id, &room_id, g, 20)
            .await
            .expect("start"),
        WalkStart::NoHistory
    );
    assert_eq!(
        completion(&pool, account_id, &room_id).await,
        Some((true, Some("no_history".to_owned()), 0))
    );
    store.delete_account_row(account_id).await.expect("cleanup");
}

/// Review finding: an empty page saves its token, but does not clear a
/// failure backoff, so a homeserver that keeps answering with empty pages
/// still parks the room. A page with events clears it.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn an_empty_page_does_not_clear_a_failure() {
    let store = migrated_store().await;
    let account_id = test_account(&store, "verify-empty").await;
    let room_id = room();
    store_events(&store, account_id, &room_id, &[1]).await;
    store
        .requeue_history_verify(account_id, Some(&room_id))
        .await
        .expect("queue");
    let g = generation(&store, account_id, &room_id).await;
    let failure = RoomBackfillFailure {
        kind: "retryable",
        retry_after: Duration::ZERO,
        parked: false,
        clear_token: false,
        timed_out: false,
    };
    store
        .record_history_verify_failure(account_id, &room_id, g, failure)
        .await
        .expect("fail");
    store
        .save_history_verify_page(account_id, &room_id, g, progress("t-1", 0, 0, 3))
        .await
        .expect("save");
    let after_empty = row(&store, account_id, &room_id).await.expect("listed");
    assert_eq!(after_empty.consecutive_failures, 1, "kept");
    assert_eq!(
        (after_empty.empty_run, after_empty.from_token.as_deref()),
        (3, Some("t-1"))
    );

    store
        .save_history_verify_page(account_id, &room_id, g, progress("t-2", 1, 0, 0))
        .await
        .expect("save");
    let after_events = row(&store, account_id, &room_id).await.expect("listed");
    assert_eq!(after_events.consecutive_failures, 0, "cleared");
    assert_eq!(after_events.empty_run, 0);
    store.delete_account_row(account_id).await.expect("cleanup");
}

/// A timed-out page adds to the walk's timeouts, which a successful page does
/// not reset (amended 2026-11-19), and a restart clears.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn timeouts_are_counted_over_the_walk_until_it_restarts() {
    let store = migrated_store().await;
    let account_id = test_account(&store, "verify-timeouts").await;
    let room_id = room();
    store_events(&store, account_id, &room_id, &[1]).await;
    store
        .requeue_history_verify(account_id, Some(&room_id))
        .await
        .expect("queue");
    let g = generation(&store, account_id, &room_id).await;
    let timed_out = RoomBackfillFailure {
        kind: "timed_out",
        retry_after: Duration::ZERO,
        parked: false,
        clear_token: false,
        timed_out: true,
    };
    store
        .record_history_verify_failure(account_id, &room_id, g, timed_out)
        .await
        .expect("fail");
    store
        .save_history_verify_page(account_id, &room_id, g, progress("t-1", 1, 0, 0))
        .await
        .expect("save");
    store
        .record_history_verify_failure(account_id, &room_id, g, timed_out)
        .await
        .expect("fail");
    let walking = row(&store, account_id, &room_id).await.expect("listed");
    assert_eq!(walking.walk_timeouts, 2);
    assert_eq!(
        walking.consecutive_failures, 1,
        "the page between reset these"
    );

    store
        .requeue_history_verify(account_id, Some(&room_id))
        .await
        .expect("restart");
    assert_eq!(
        row(&store, account_id, &room_id)
            .await
            .expect("listed")
            .walk_timeouts,
        0
    );
    store.delete_account_row(account_id).await.expect("cleanup");
}
