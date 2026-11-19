//! The ADR 0140 delivery step 2 tests. The floor rule is unit-tested; the
//! rest store real sliding-sync responses from step 1's stand-in homeserver
//! with no gap watcher, so their holes have no rows, as holes from before
//! ADR 0140 have none, and walk them with the real verifier.
//!
//! The DB-gated tests require Postgres (`DATABASE_URL`) and are `#[ignore]`d
//! by default.

use std::time::Duration;

use serde_json::json;
use uuid::Uuid;

use super::*;
use crate::backfill::gaps::tests::{deliver, range, tokens, Harness};
use crate::backfill::gaps::GapSignal;

fn at(ts: i64) -> FloorEvent {
    FloorEvent {
        stored: false,
        origin_ts: Some(ts),
    }
}

#[test]
fn the_floor_run_grows_only_on_pages_wholly_below_stored_history() {
    let floor = Some(100);
    assert_eq!(next_floor_run(0, &[at(10), at(99)], floor, 4), 2);
    assert_eq!(next_floor_run(498, &[at(10), at(99)], floor, 4), 500);
    // One event at or above the floor, or one already stored, resets it: the
    // walk is still inside history Axon holds.
    assert_eq!(next_floor_run(40, &[at(10), at(100)], floor, 4), 0);
    let stored = FloorEvent {
        stored: true,
        origin_ts: Some(10),
    };
    assert_eq!(next_floor_run(40, &[at(10), stored], floor, 4), 0);
    // An event without a timestamp cannot be placed, so it resets too.
    let undated = FloorEvent {
        stored: false,
        origin_ts: None,
    };
    assert_eq!(next_floor_run(40, &[undated], floor, 4), 0);
    // An empty page counts as a full one, so empty pages cannot hold the walk
    // open; a room with no floor never accumulates a run.
    assert_eq!(next_floor_run(40, &[], floor, 4), 44);
    assert_eq!(next_floor_run(40, &[at(10)], None, 4), 0);
}

fn params(h: &Harness) -> BackfillParams {
    let mut params = h.params(false);
    params.verify_enabled = true;
    params
}

/// Queue the room's walk, as the migration's seed does for every room with
/// stored events.
async fn queue(h: &Harness, tag: &str) {
    assert_eq!(
        h.store
            .requeue_history_verify(h.account_id, Some(h.room(tag)))
            .await
            .unwrap(),
        1
    );
}

/// Walk every walkable room to its end, as the backfill task's verification
/// pages would.
async fn walk_with(h: &Harness, params: &BackfillParams) -> Vec<VerifyStep> {
    let mut verifier = Verifier::new(&h.client, &h.ctx, params, &h.health, &h.gate, &h.cancel);
    verifier.refresh().await;
    let mut steps = Vec::new();
    loop {
        let step = verifier.step().await;
        steps.push(step);
        if matches!(step, VerifyStep::Idle | VerifyStep::Abandoned) {
            return steps;
        }
        assert!(steps.len() < 2_000, "the walk never closed");
    }
}

async fn walk(h: &Harness) -> Vec<VerifyStep> {
    walk_with(h, &params(h)).await
}

/// The row's `complete`, `stop_reason` and `events_recovered`.
async fn outcome(h: &Harness, tag: &str) -> (bool, Option<String>, i64) {
    sqlx_core::query_as::query_as(
        "SELECT complete, stop_reason, events_recovered FROM room_history_verify \
         WHERE account_id = $1 AND room_id = $2",
    )
    .bind(h.account_id)
    .bind(h.room(tag))
    .fetch_one(h.store.pool())
    .await
    .unwrap()
}

fn closed(reason: &str, recovered: i64) -> (bool, Option<String>, i64) {
    (true, Some(reason.to_owned()), recovered)
}

/// Remove the event at upstream position `at`, as a homeserver that purged
/// it or filters it no longer serves it.
fn withhold(h: &Harness, tag: &str, at: usize) {
    let room_id = h.room(tag).to_owned();
    h.upstream
        .lock()
        .unwrap()
        .rooms
        .get_mut(&room_id)
        .unwrap()
        .remove(at);
}

/// Add `n` events dated `origin_ts`, as an import's events carry timestamps
/// from long before the room's first stored event.
fn append_dated(h: &Harness, tag: &str, n: usize, origin_ts: u64) {
    let room_id = h.room(tag).to_owned();
    let mut up = h.upstream.lock().unwrap();
    let events = up.rooms.get_mut(&room_id).unwrap();
    let start = events.len();
    for i in start..start + n {
        events.push(json!({
            "type": "m.room.message",
            "sender": h.user,
            "event_id": Harness::event_id(tag, i),
            "origin_server_ts": origin_ts,
            "content": {"msgtype": "m.text", "body": format!("{tag} imported {i}")},
        }));
    }
}

/// Holes that limited syncs left before ADR 0140 recorded them have no gap
/// row. The walk reads the room from its live end, fills both, and closes on
/// the page holding the room's first stored event, without walking on into
/// older history.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_walk_fills_holes_that_have_no_rows_and_stops_at_the_first_stored_event() {
    let mut h = Harness::new(&["a"]).await;
    h.append("a", 10);
    h.sync(&[deliver("a", 5, 10)]).await;
    h.append("a", 20);
    h.sync(&[deliver("a", 25, 30).limited()]).await;
    h.append("a", 20);
    h.sync(&[deliver("a", 45, 50).limited()]).await;
    assert!(h.gaps().await.is_empty(), "no watcher, so no gap rows");
    assert_eq!(
        h.stored("a").await,
        &(&range(5, 10) | &range(25, 30)) | &range(45, 50)
    );

    queue(&h, "a").await;
    walk(&h).await;
    // The page [2, 6) holds event 5, the room's first stored event; it is
    // written in full.
    assert_eq!(h.stored("a").await, range(2, 50), "both holes filled");
    assert_eq!(
        h.take_requests("a"),
        [
            vec![None],
            tokens(&[46, 42, 38, 34, 30, 26, 22, 18, 14, 10, 6])
        ]
        .concat()
    );
    assert_eq!(outcome(&h, "a").await, closed("first_stored_event", 33));

    // Done once: a walk that has closed is not walked again.
    assert_eq!(walk(&h).await, [VerifyStep::Idle]);
    assert!(h.take_requests("a").is_empty());
    h.finish().await;
}

/// The amendment's import-room finding: when the room's first window was
/// shorter than `timeline_limit`, its lowest-id events include the next
/// response's batch. A walk that stopped on any of them would close on its
/// first page, above the hole; it stops on the single lowest-id event.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_short_first_window_does_not_stop_the_walk_above_a_hole() {
    let mut h = Harness::new(&["a"]).await;
    h.append("a", 10);
    h.sync(&[deliver("a", 9, 10)]).await;
    h.append("a", 20);
    h.sync(&[deliver("a", 25, 30).limited()]).await;

    queue(&h, "a").await;
    walk(&h).await;
    assert_eq!(outcome(&h, "a").await, closed("first_stored_event", 18));
    // The closing page [6, 10) holds event 9.
    assert_eq!(h.stored("a").await, range(6, 30), "the hole filled");
    h.finish().await;
}

/// The amendment's `#matrix:matrix.org` case: the homeserver no longer serves
/// the room's first stored event. Without the floor the walk would read the
/// whole room to its start; it closes after 500 unstored events older than
/// the room's first events instead.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_first_stored_event_the_homeserver_no_longer_serves_stops_at_the_floor() {
    let mut h = Harness::new(&["a"]).await;
    h.append("a", 620);
    h.sync(&[deliver("a", 615, 620)]).await;
    withhold(&h, "a", 615);

    queue(&h, "a").await;
    walk(&h).await;
    let (complete, reason, _) = outcome(&h, "a").await;
    assert_eq!((complete, reason.as_deref()), (true, Some("floor")));
    // 125 pages of 4 below the stored events, then closed: nothing near the
    // room's start was requested.
    assert_eq!(
        h.stored("a").await,
        range(115, 620),
        "500 events past the stored ones, no more"
    );
    assert_eq!(h.take_requests("a").len(), 1 + 125);
    h.finish().await;
}

/// A room whose history ends before the floor run does walks to its start,
/// which is logged at `WARN`.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_walk_that_never_meets_its_stop_ends_at_the_room_start() {
    let mut h = Harness::new(&["a"]).await;
    h.append("a", 30);
    h.sync(&[deliver("a", 25, 30)]).await;
    withhold(&h, "a", 25);

    queue(&h, "a").await;
    walk(&h).await;
    assert_eq!(outcome(&h, "a").await, closed("room_start", 25));
    assert_eq!(h.stored("a").await, range(0, 30));
    h.finish().await;
}

/// The 2026-10-18 import's shape: 600 events dated years before the room's
/// first stored event, of which live sync stored the newest 5 of every 15.
/// Every page of the import is older than the floor, more than 500 in a row,
/// but the stored runs among them keep resetting the floor run, so the walk
/// goes on through every hole to the first stored event.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn old_dated_events_among_stored_ones_do_not_stop_the_walk() {
    let mut h = Harness::new(&["a"]).await;
    h.append("a", 10);
    h.sync(&[deliver("a", 5, 10)]).await;
    for block in 0..40 {
        let start = 10 + block * 15;
        append_dated(&h, "a", 15, 1_000);
        h.sync(&[deliver("a", start + 10, start + 15).limited()])
            .await;
    }
    assert_eq!(h.stored("a").await.len(), 5 + 200);

    queue(&h, "a").await;
    walk(&h).await;
    // The walk's pages end at 610, 606, ..., so the closing page is [2, 6).
    assert_eq!(outcome(&h, "a").await, closed("first_stored_event", 403));
    assert_eq!(h.stored("a").await, range(2, 610), "every hole filled");
    h.finish().await;
}

/// A room with an open gap row waits for it, since the row is filling the
/// same newest history; other rooms are walked meanwhile. Once the row
/// closes, the room is walked too.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_room_with_an_open_gap_row_waits_for_it() {
    let mut h = Harness::new(&["a", "b"]).await;
    let mut watcher = h.watcher().await;
    h.append("a", 10);
    h.append("b", 10);
    let first = h.sync(&[deliver("a", 5, 10), deliver("b", 5, 10)]).await;
    watcher.observe(&first).await;
    h.append("a", 20);
    let limited = h.sync(&[deliver("a", 25, 30).limited()]).await;
    watcher.observe(&limited).await;
    assert_eq!(h.gaps().await.len(), 1);

    queue(&h, "a").await;
    queue(&h, "b").await;
    walk(&h).await;
    assert!(h.take_requests("a").is_empty(), "held by its gap row");
    assert!(outcome(&h, "b").await.0, "the other room was walked");
    assert!(!outcome(&h, "a").await.0);

    h.fill().await;
    assert!(h.gaps().await.is_empty());
    walk(&h).await;
    assert!(outcome(&h, "a").await.0);
    assert_eq!(h.stored("a").await, range(2, 30));
    h.finish().await;
}

/// A rejected token sends the walk back to the live end with its stop
/// points, as for a backfill cursor (ADR 0121); the walk then completes.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_rejected_token_restarts_the_walk_from_the_live_end() {
    let mut h = Harness::new(&["a"]).await;
    h.append("a", 10);
    h.sync(&[deliver("a", 5, 10)]).await;
    h.append("a", 20);
    h.sync(&[deliver("a", 25, 30).limited()]).await;
    queue(&h, "a").await;

    let params = params(&h);
    let mut verifier = Verifier::new(&h.client, &h.ctx, &params, &h.health, &h.gate, &h.cancel);
    verifier.refresh().await;
    assert_eq!(verifier.step().await, VerifyStep::Paged);
    h.upstream.lock().unwrap().rejected.insert("s26".to_owned());
    assert_eq!(verifier.step().await, VerifyStep::Failed);
    h.upstream.lock().unwrap().rejected.clear();

    walk(&h).await;
    let requests = h.take_requests("a");
    assert_eq!(
        requests[..4],
        [None, Some("s26".to_owned()), None, Some("s26".to_owned())]
    );
    assert!(outcome(&h, "a").await.0);
    assert_eq!(h.stored("a").await, range(2, 30));
    h.finish().await;
}

/// Only joined rooms are walked. A row in a room the account is not in waits,
/// and is marked for status; with the switch off nothing is walked at all.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn rooms_not_joined_wait_and_the_switch_turns_the_pass_off() {
    let mut h = Harness::new(&["a"]).await;
    h.append("a", 10);
    h.sync(&[deliver("a", 5, 10)]).await;
    queue(&h, "a").await;
    let departed = format!("!departed-{}:localhost", Uuid::new_v4());
    sqlx_core::query::query(
        "INSERT INTO room_history_verify (account_id, room_id) VALUES ($1, $2)",
    )
    .bind(h.account_id)
    .bind(&departed)
    .execute(h.store.pool())
    .await
    .unwrap();

    let mut off = params(&h);
    off.verify_enabled = false;
    assert_eq!(walk_with(&h, &off).await, [VerifyStep::Idle]);
    assert!(h.take_requests("a").is_empty(), "the switch is off");

    walk(&h).await;
    assert!(outcome(&h, "a").await.0);
    let rows = h.store.list_history_verify(h.account_id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].room_id, departed);
    assert!(rows[0].not_joined, "recorded for status");
    h.finish().await;
}

/// The backfill task runs the pass beside ordinary backfill, so neither
/// starves the other: room `a`'s backfill is already complete and only the
/// walk can fill its hole, while room `b` has no cursor yet and only ordinary
/// backfill pages it to its start. Both finish.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn the_backfill_task_runs_the_pass_beside_ordinary_backfill() {
    let mut h = Harness::new(&["a", "b"]).await;
    h.append("a", 10);
    h.append("b", 10);
    h.sync(&[deliver("a", 5, 10), deliver("b", 5, 10)]).await;
    h.append("a", 20);
    h.sync(&[deliver("a", 25, 30).limited()]).await;
    h.store
        .save_room_backfill(
            h.account_id,
            h.room("a"),
            None,
            true,
            0,
            axon_store::PageSpeed::Fast,
        )
        .await
        .unwrap();
    queue(&h, "a").await;

    let mut params = params(&h);
    params.enabled = true;
    let cancel = h.cancel.child_token();
    let task = tokio::spawn(crate::backfill::run(
        h.client.clone(),
        h.ctx.clone(),
        params,
        h.health.clone(),
        Arc::clone(&h.gate),
        Arc::new(GapSignal::default()),
        cancel.clone(),
    ));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let verified = outcome(&h, "a").await.0;
        let backfilled = h
            .store
            .get_room_backfill(h.account_id, h.room("b"))
            .await
            .unwrap()
            .is_some_and(|state| state.complete);
        if verified && backfilled {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "verified {verified}, backfilled {backfilled}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    cancel.cancel();
    task.await.unwrap();
    assert_eq!(
        h.stored("a").await,
        range(2, 30),
        "the walk filled the hole"
    );
    assert_eq!(
        h.stored("b").await,
        range(0, 10),
        "backfill reached the start"
    );
    h.finish().await;
}
