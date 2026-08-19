//! DB-gated tests for the M10 backfill cursor and room purge.
//!
//! Require a running Postgres and are `#[ignore]`d by default. Run with:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store -- --ignored
//! ```

mod common;

use axon_store::room_purge_sentinel;
use common::{insert_message, migrated_store, raw_pool, test_account};
use sqlx_postgres::PgPool;
use uuid::Uuid;

async fn count(pool: &PgPool, sql: &str, account_id: Uuid, room_id: &str) -> i64 {
    let (n,): (i64,) = sqlx_core::query_as::query_as::<_, (i64,)>(sql)
        .bind(account_id)
        .bind(room_id)
        .fetch_one(pool)
        .await
        .expect("count query");
    n
}

async fn seed_room_state(pool: &PgPool, account_id: Uuid, room_id: &str) {
    sqlx_core::query::query(
        "INSERT INTO room_state \
             (account_id, room_id, event_type, state_key, event_id, sender, origin_ts, content) \
         VALUES ($1, $2, 'm.room.name', '', $3, '@alice:localhost', 1, '{}'::jsonb)",
    )
    .bind(account_id)
    .bind(room_id)
    .bind(format!("$state-{}:localhost", Uuid::new_v4()))
    .execute(pool)
    .await
    .expect("seed room_state");
}

async fn seed_room_account_data(pool: &PgPool, account_id: Uuid, room_id: &str) {
    sqlx_core::query::query(
        "INSERT INTO account_data (account_id, room_id, event_type, content) \
         VALUES ($1, $2, 'm.fully_read', '{}'::jsonb)",
    )
    .bind(account_id)
    .bind(room_id)
    .execute(pool)
    .await
    .expect("seed account_data");
}

/// Record the local user's own membership in a room (upsert the `m.room.member`
/// tuple keyed by their user id), so the membership filter has a leave/ban/join
/// signal to act on.
async fn set_membership(pool: &PgPool, account_id: Uuid, room_id: &str, user_id: &str, m: &str) {
    sqlx_core::query::query(
        "INSERT INTO room_state \
             (account_id, room_id, event_type, state_key, event_id, sender, origin_ts, content) \
         VALUES ($1, $2, 'm.room.member', $3, $4, $3, 1, jsonb_build_object('membership', $5::text)) \
         ON CONFLICT (account_id, room_id, event_type, state_key) DO UPDATE \
             SET content = EXCLUDED.content",
    )
    .bind(account_id)
    .bind(room_id)
    .bind(user_id)
    .bind(format!("$m-{}:localhost", Uuid::new_v4()))
    .bind(m)
    .execute(pool)
    .await
    .expect("set membership");
}

/// No cursor exists until the first page is saved.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn get_room_backfill_is_none_before_first_write() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "bf").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());

    let got = store
        .get_room_backfill(account_id, &room_id)
        .await
        .expect("get");
    assert!(got.is_none(), "no row should exist before first save");

    common::cleanup_account(&pool, account_id).await;
}

/// A first save inserts the cursor; a later save updates the token/complete flag
/// and *accumulates* the running event count rather than overwriting it.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn save_room_backfill_roundtrips_and_accumulates() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "bf").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());

    store
        .save_room_backfill(account_id, &room_id, Some("tok-1"), false, 40)
        .await
        .expect("first save");
    let first = store
        .get_room_backfill(account_id, &room_id)
        .await
        .expect("get")
        .expect("row exists");
    assert_eq!(first.oldest_seen_token.as_deref(), Some("tok-1"));
    assert!(!first.complete);
    assert_eq!(first.events_backfilled, 40);

    store
        .save_room_backfill(account_id, &room_id, Some("tok-2"), false, 35)
        .await
        .expect("second save");
    let second = store
        .get_room_backfill(account_id, &room_id)
        .await
        .expect("get")
        .expect("row exists");
    assert_eq!(second.oldest_seen_token.as_deref(), Some("tok-2"));
    assert_eq!(
        second.events_backfilled, 75,
        "count accumulates across pages"
    );

    // Exhaustion: end came back NULL -> complete, no further token.
    store
        .save_room_backfill(account_id, &room_id, None, true, 5)
        .await
        .expect("final save");
    let done = store
        .get_room_backfill(account_id, &room_id)
        .await
        .expect("get")
        .expect("row exists");
    assert!(done.complete, "room marked complete");
    assert!(done.oldest_seen_token.is_none());
    assert_eq!(done.events_backfilled, 80);

    common::cleanup_account(&pool, account_id).await;
}

/// Deleting the cursor removes the row so a re-join backfills afresh.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn delete_room_backfill_removes_the_cursor() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "bf").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());

    store
        .save_room_backfill(account_id, &room_id, Some("tok"), false, 10)
        .await
        .expect("save");
    store
        .delete_room_backfill(account_id, &room_id)
        .await
        .expect("delete");
    assert!(store
        .get_room_backfill(account_id, &room_id)
        .await
        .expect("get")
        .is_none());

    common::cleanup_account(&pool, account_id).await;
}

/// `purge_room` removes every stored trace of the room for the account and
/// appends the room-purge sentinel to the search outbox.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn purge_room_clears_stored_state_and_enqueues_search_purge() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "purge").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());

    insert_message(&store, account_id, &room_id, 1, "hello world").await;
    insert_message(&store, account_id, &room_id, 2, "second message").await;
    seed_room_state(&pool, account_id, &room_id).await;
    seed_room_account_data(&pool, account_id, &room_id).await;
    store
        .save_room_backfill(account_id, &room_id, Some("tok"), false, 2)
        .await
        .expect("save backfill");

    store.purge_room(account_id, &room_id).await.expect("purge");

    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM events WHERE account_id=$1 AND room_id=$2",
            account_id,
            &room_id
        )
        .await,
        0,
        "events removed"
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM room_state WHERE account_id=$1 AND room_id=$2",
            account_id,
            &room_id
        )
        .await,
        0,
        "room_state removed"
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM account_data WHERE account_id=$1 AND room_id=$2",
            account_id,
            &room_id
        )
        .await,
        0,
        "room account_data removed"
    );
    assert!(
        store
            .get_room_backfill(account_id, &room_id)
            .await
            .expect("get")
            .is_none(),
        "backfill cursor removed"
    );

    // The room-purge sentinel is enqueued for the indexer.
    let sentinel = room_purge_sentinel(&room_id);
    let n = count(
        &pool,
        "SELECT count(*) FROM search_outbox WHERE account_id=$1 AND event_id=$2",
        account_id,
        &sentinel,
    )
    .await;
    assert_eq!(n, 1, "one room-purge obligation appended");

    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM room_summaries WHERE account_id=$1 AND room_id=$2",
            account_id,
            &room_id
        )
        .await,
        0,
        "room_summaries removed"
    );

    common::cleanup_account(&pool, account_id).await;
}

/// `purge_room` is scoped: it never touches another room, nor another account's
/// copy of the same room.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn purge_room_leaves_other_rooms_and_accounts_intact() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_a = test_account(&store, "purge-a").await;
    let account_b = test_account(&store, "purge-b").await;
    let shared_room = format!("!shared-{}:localhost", Uuid::new_v4());
    let other_room = format!("!other-{}:localhost", Uuid::new_v4());

    insert_message(&store, account_a, &shared_room, 1, "a in shared").await;
    insert_message(&store, account_a, &other_room, 1, "a in other").await;
    insert_message(&store, account_b, &shared_room, 1, "b in shared").await;

    store
        .purge_room(account_a, &shared_room)
        .await
        .expect("purge");

    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM events WHERE account_id=$1 AND room_id=$2",
            account_a,
            &shared_room
        )
        .await,
        0,
        "purged account's copy of the shared room is gone"
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM events WHERE account_id=$1 AND room_id=$2",
            account_a,
            &other_room
        )
        .await,
        1,
        "the account's other room is untouched"
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM events WHERE account_id=$1 AND room_id=$2",
            account_b,
            &shared_room
        )
        .await,
        1,
        "the other account's copy of the shared room is untouched"
    );

    common::cleanup_account(&pool, account_a).await;
    common::cleanup_account(&pool, account_b).await;
}

/// Re-purging an already-purged (now empty) room enqueues no additional search
/// obligation — the guard that keeps the leave state re-observed on later syncs
/// from growing `search_outbox` unboundedly.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn purge_room_rerun_enqueues_no_duplicate_obligation() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "purge-idem").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());
    insert_message(&store, account_id, &room_id, 1, "hello").await;

    store
        .purge_room(account_id, &room_id)
        .await
        .expect("first purge");
    store
        .purge_room(account_id, &room_id)
        .await
        .expect("second purge (no-op)");

    let sentinel = room_purge_sentinel(&room_id);
    let n = count(
        &pool,
        "SELECT count(*) FROM search_outbox WHERE account_id=$1 AND event_id=$2",
        account_id,
        &sentinel,
    )
    .await;
    assert_eq!(
        n, 1,
        "re-purging an empty room enqueues no extra obligation"
    );

    common::cleanup_account(&pool, account_id).await;
}

/// The membership filter (ADR 0044): `get_event_if_joined` hides events in a room
/// the local user has left, and shows them again after a re-join — while plain
/// `get_event` is unaffected. This is the non-destructive default that keeps
/// left-room content out of search without deleting anything.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn get_event_if_joined_reflects_membership() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "member").await;
    let user_id = store
        .get_account(account_id)
        .await
        .expect("get account")
        .expect("account exists")
        .user_id;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());
    let event_id = insert_message(&store, account_id, &room_id, 1, "still here").await;

    // Joined (no membership row yet): both reads see the event.
    assert!(store
        .get_event(account_id, &event_id)
        .await
        .unwrap()
        .is_some());
    assert!(store
        .get_event_if_joined(account_id, &event_id)
        .await
        .unwrap()
        .is_some());

    // Leave: the plain read still returns it; the joined-filtered read hides it.
    set_membership(&pool, account_id, &room_id, &user_id, "leave").await;
    assert!(
        store
            .get_event(account_id, &event_id)
            .await
            .unwrap()
            .is_some(),
        "unfiltered read is unaffected by membership"
    );
    assert!(
        store
            .get_event_if_joined(account_id, &event_id)
            .await
            .unwrap()
            .is_none(),
        "left-room event is hidden from the membership-filtered read"
    );

    // Re-join: it reappears (reversible, nothing was deleted).
    set_membership(&pool, account_id, &room_id, &user_id, "join").await;
    assert!(
        store
            .get_event_if_joined(account_id, &event_id)
            .await
            .unwrap()
            .is_some(),
        "re-joining restores the event to search"
    );

    common::cleanup_account(&pool, account_id).await;
}

/// Re-upserting the same event is a no-op that appends no duplicate search-outbox
/// obligation — the property the backfill engine relies on for idempotent re-runs.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn reingesting_an_event_appends_no_duplicate_outbox_row() {
    use axon_store::NewEvent;

    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "idem").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());
    let event_id = format!("$evt-{}:localhost", Uuid::new_v4());
    let content = serde_json::json!({ "msgtype": "m.text", "body": "once" });
    let ev = NewEvent {
        event_id: &event_id,
        room_id: &room_id,
        account_id,
        sender: "@alice:localhost",
        origin_ts: 1,
        event_type: "m.room.message",
        content: Some(content.clone()),
        raw_event: serde_json::json!({ "type": "m.room.message", "content": content }),
        megolm_session_id: None,
        redacts: None,
        relates_to: None,
        decrypted_body_text: Some("once"),
    };

    store.upsert_event(&ev).await.expect("first upsert");
    store
        .upsert_event(&ev)
        .await
        .expect("second upsert (no-op)");

    let events = count(
        &pool,
        "SELECT count(*) FROM events WHERE account_id=$1 AND room_id=$2",
        account_id,
        &room_id,
    )
    .await;
    assert_eq!(events, 1, "one events row despite two upserts");

    let (outbox,): (i64,) = sqlx_core::query_as::query_as::<_, (i64,)>(
        "SELECT count(*) FROM search_outbox WHERE account_id=$1 AND event_id=$2",
    )
    .bind(account_id)
    .bind(&event_id)
    .fetch_one(&pool)
    .await
    .expect("outbox count");
    assert_eq!(
        outbox, 1,
        "second (conflicting) upsert appends no outbox row"
    );

    common::cleanup_account(&pool, account_id).await;
}

/// `list_room_backfill` returns every room's cursor for the account in one call —
/// the per-sweep batch read the backfill task uses instead of a per-room lookup.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn list_room_backfill_returns_all_cursors() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "list").await;
    let room_a = format!("!a-{}:localhost", Uuid::new_v4());
    let room_b = format!("!b-{}:localhost", Uuid::new_v4());

    store
        .save_room_backfill(account_id, &room_a, Some("tok-a"), false, 10)
        .await
        .expect("save a");
    store
        .save_room_backfill(account_id, &room_b, None, true, 20)
        .await
        .expect("save b");

    let map = store.list_room_backfill(account_id).await.expect("list");
    assert_eq!(map.len(), 2);
    assert_eq!(map[&room_a].oldest_seen_token.as_deref(), Some("tok-a"));
    assert!(!map[&room_a].complete);
    assert!(map[&room_b].complete);
    assert_eq!(map[&room_b].events_backfilled, 20);

    common::cleanup_account(&pool, account_id).await;
}

/// `backfill_progress` counts the account's events and its joined rooms, and how
/// many are fully backfilled — excluding left rooms (the `GET /v1/status` feed).
#[tokio::test]
#[ignore = "requires Postgres"]
async fn backfill_progress_counts_joined_rooms_and_completion() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "progress").await;
    let user_id = store
        .get_account(account_id)
        .await
        .expect("get account")
        .expect("account exists")
        .user_id;
    let room_a = format!("!a-{}:localhost", Uuid::new_v4());
    let room_b = format!("!b-{}:localhost", Uuid::new_v4());
    let room_left = format!("!left-{}:localhost", Uuid::new_v4());

    insert_message(&store, account_id, &room_a, 1, "a1").await;
    insert_message(&store, account_id, &room_a, 2, "a2").await;
    insert_message(&store, account_id, &room_b, 1, "b1").await;
    insert_message(&store, account_id, &room_left, 1, "gone").await;
    // Room A fully backfilled; B still ongoing (no row); left room excluded.
    store
        .save_room_backfill(account_id, &room_a, None, true, 2)
        .await
        .expect("complete a");
    set_membership(&pool, account_id, &room_left, &user_id, "leave").await;

    let progress = store.backfill_progress().await.expect("progress");
    let row = progress
        .into_iter()
        .find(|p| p.account_id == account_id)
        .expect("account present");
    assert_eq!(row.events_total, 4, "all events counted, incl. left room");
    assert_eq!(row.rooms_total, 2, "left room excluded from joined rooms");
    assert_eq!(row.rooms_backfilled, 1, "only room A is complete");

    common::cleanup_account(&pool, account_id).await;
}
