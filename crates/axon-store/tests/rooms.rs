//! Integration tests for the room list (`list_rooms`) and single-event read
//! (`get_event`).
//!
//! Like the other store integration tests these need a running Postgres and are
//! `#[ignore]`d by default. Run them with:
//!
//! ```sh
//! docker compose up -d postgres
//! # 5432 is the default; use your compose host port (e.g. 5433 if 5432 is taken).
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store -- --ignored
//! ```

mod common;

use axon_store::{AccountState, NewEvent, RoomStateUpsert, Store};
use common::{insert_message, test_account};
use serde_json::{json, Value};
use uuid::Uuid;

/// Redact `target` with an `m.room.redaction` event, returning the redaction's
/// event_id.
async fn insert_redaction(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    origin_ts: i64,
    target: &str,
) -> String {
    let event_id = format!("$red-{}:localhost", Uuid::new_v4());
    store
        .upsert_event(&NewEvent {
            event_id: &event_id,
            room_id,
            account_id,
            sender: "@alice:localhost",
            origin_ts,
            event_type: "m.room.redaction",
            content: Some(json!({})),
            raw_event: json!({ "type": "m.room.redaction", "redacts": target }),
            megolm_session_id: None,
            redacts: Some(target),
            relates_to: None,
            decrypted_body_text: None,
        })
        .await
        .expect("insert redaction");
    event_id
}

async fn set_state(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    event_type: &str,
    content: Value,
) {
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id,
            event_type,
            state_key: "",
            event_id: &format!("$state-{}:localhost", Uuid::new_v4()),
            sender: "@alice:localhost",
            origin_ts: 1_000,
            content: Some(content),
        })
        .await
        .expect("set state");
}

/// `list_rooms` orders by most-recent activity, carries the latest event id, and
/// resolves the four display fields from current room state.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn list_rooms_orders_by_activity_with_summary_fields() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "rooms").await;
    let room_a = format!("!a-{}:localhost", Uuid::new_v4());
    let room_b = format!("!b-{}:localhost", Uuid::new_v4());

    // Room A: two events; the newest (ts 2000) is the activity marker.
    insert_message(&store, account_id, &room_a, 1_000, "first").await;
    let a_latest = insert_message(&store, account_id, &room_a, 2_000, "second").await;
    set_state(
        &store,
        account_id,
        &room_a,
        "m.room.name",
        json!({ "name": "Alpha" }),
    )
    .await;
    set_state(
        &store,
        account_id,
        &room_a,
        "m.room.topic",
        json!({ "topic": "about a" }),
    )
    .await;
    set_state(
        &store,
        account_id,
        &room_a,
        "m.room.avatar",
        json!({ "url": "mxc://hs/a" }),
    )
    .await;
    set_state(
        &store,
        account_id,
        &room_a,
        "m.room.canonical_alias",
        json!({ "alias": "#alpha:localhost" }),
    )
    .await;
    set_state(
        &store,
        account_id,
        &room_a,
        "m.room.create",
        json!({ "type": "m.space" }),
    )
    .await;

    // Room B: a single, newer event (ts 3000) and no state — sorts first.
    let b_latest = insert_message(&store, account_id, &room_b, 3_000, "newest").await;

    // Room A has a seeded unread-counts row (issue #313); room B has none yet.
    store
        .upsert_room_unread_counts(account_id, &room_a, 4, 1)
        .await
        .expect("seed unread counts");

    let rooms = store.list_rooms(Some(account_id)).await.expect("list");
    assert_eq!(rooms.len(), 2, "exactly the two rooms for this account");

    // Newest-activity first: Room B (3000) before Room A (2000).
    assert_eq!(rooms[0].room_id, room_b);
    assert!(rooms[0].account_user_id.starts_with("@rooms-"));
    assert_eq!(rooms[0].last_activity_ts, 3_000);
    assert_eq!(rooms[0].last_event_id.as_deref(), Some(b_latest.as_str()));
    assert!(rooms[0].name.is_none(), "room B has no name state");
    assert_eq!(rooms[0].room_type, None);
    assert_eq!(
        rooms[0].notification_count, 0,
        "room B has no unread-counts row yet — reads as 0, not NULL"
    );
    assert_eq!(rooms[0].highlight_count, 0);

    assert_eq!(rooms[1].room_id, room_a);
    assert!(rooms[1].account_user_id.starts_with("@rooms-"));
    assert_eq!(rooms[1].last_activity_ts, 2_000);
    assert_eq!(rooms[1].last_event_id.as_deref(), Some(a_latest.as_str()));
    assert_eq!(rooms[1].name.as_deref(), Some("Alpha"));
    assert_eq!(rooms[1].topic.as_deref(), Some("about a"));
    assert_eq!(rooms[1].avatar_url.as_deref(), Some("mxc://hs/a"));
    assert_eq!(
        rooms[1].canonical_alias.as_deref(),
        Some("#alpha:localhost")
    );
    assert_eq!(rooms[1].room_type.as_deref(), Some("m.space"));
    assert_eq!(rooms[1].notification_count, 4);
    assert_eq!(rooms[1].highlight_count, 1);

    common::cleanup_account(&pool, account_id).await;
}

/// `list_rooms` activity/`last_event_id` only advance on content-bearing
/// events: a redaction landing after the last message must not
/// bump the room's sort position or point `last_event_id` at itself. A room
/// with no content-bearing event at all (only a redaction, in this case)
/// still appears in the list rather than vanishing, falling back to the
/// unfiltered latest event.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn list_rooms_activity_ignores_non_content_events() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "activity").await;

    // Room A: a message, then a later redaction of some other event — the
    // redaction is newer (ts 5000) but must not become the activity marker.
    let room_a = format!("!act-a-{}:localhost", Uuid::new_v4());
    let a_msg = insert_message(&store, account_id, &room_a, 1_000, "hi").await;
    insert_redaction(&store, account_id, &room_a, 5_000, &a_msg).await;

    // Room B: no messages at all, just a redaction (of an id that doesn't even
    // need to exist for this test) — must still appear, sorted by that event.
    let room_b = format!("!act-b-{}:localhost", Uuid::new_v4());
    let b_redaction =
        insert_redaction(&store, account_id, &room_b, 2_000, "$nonexistent:localhost").await;

    let rooms = store.list_rooms(Some(account_id)).await.expect("list");
    let a = rooms
        .iter()
        .find(|r| r.room_id == room_a)
        .expect("room a present");
    assert_eq!(
        a.last_activity_ts, 1_000,
        "the redaction (ts 5000) must not count as activity"
    );
    assert_eq!(a.last_event_id.as_deref(), Some(a_msg.as_str()));

    let b = rooms
        .iter()
        .find(|r| r.room_id == room_b)
        .expect("message-less room still appears, not hidden");
    assert_eq!(
        b.last_activity_ts, 2_000,
        "falls back to the unfiltered latest event when there's no content-bearing one"
    );
    assert_eq!(b.last_event_id.as_deref(), Some(b_redaction.as_str()));

    common::cleanup_account(&pool, account_id).await;
}

/// `upsert_room_unread_counts` inserts on first write and overwrites (rather
/// than accumulating) on a later write for the same `(account_id, room_id)` —
/// the watcher always supplies the full current value, never a delta.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn upsert_room_unread_counts_inserts_then_overwrites() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "unread").await;
    let room_id = format!("!u-{}:localhost", Uuid::new_v4());

    store
        .upsert_room_unread_counts(account_id, &room_id, 2, 0)
        .await
        .expect("insert");
    let rooms = store.list_rooms(Some(account_id)).await.expect("list");
    // No events for this room, so it doesn't appear in the room list itself —
    // read the row directly to confirm the insert landed.
    assert!(rooms.is_empty(), "no events yet, so no room-list entry");
    let (n, h): (i64, i64) = sqlx_core::query_as::query_as(
        "SELECT notification_count, highlight_count FROM room_unread_counts \
         WHERE account_id = $1 AND room_id = $2",
    )
    .bind(account_id)
    .bind(&room_id)
    .fetch_one(&pool)
    .await
    .expect("read seeded row");
    assert_eq!((n, h), (2, 0));

    store
        .upsert_room_unread_counts(account_id, &room_id, 5, 3)
        .await
        .expect("overwrite");
    let (n, h): (i64, i64) = sqlx_core::query_as::query_as(
        "SELECT notification_count, highlight_count FROM room_unread_counts \
         WHERE account_id = $1 AND room_id = $2",
    )
    .bind(account_id)
    .bind(&room_id)
    .fetch_one(&pool)
    .await
    .expect("read overwritten row");
    assert_eq!((n, h), (5, 3), "overwrites rather than accumulating");

    common::cleanup_account(&pool, account_id).await;
}

/// Set the local user's `m.room.member` state in a room to `membership`
/// (state_key = the account's own user id), as a leave/ban/join would.
async fn set_membership(
    store: &Store,
    account_id: Uuid,
    account_user_id: &str,
    room_id: &str,
    membership: &str,
) {
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id,
            event_type: "m.room.member",
            state_key: account_user_id,
            event_id: &format!("$mem-{}:localhost", Uuid::new_v4()),
            sender: account_user_id,
            origin_ts: 2_000,
            content: Some(json!({ "membership": membership })),
        })
        .await
        .expect("set membership");
}

/// `list_rooms` hides rooms the local user has left or been banned from, but
/// keeps joined rooms and rooms with no membership row for the local user
/// (ADR 0037). Membership is the local user's current `m.room.member` value.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn list_rooms_excludes_left_and_banned_rooms() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "left").await;

    // Recover the account's own user id (test_account randomizes it) — it is the
    // state_key of the local user's membership row.
    let me = store.list_rooms(Some(account_id)).await.expect("list");
    assert!(me.is_empty(), "no rooms yet");
    let user_id: String =
        sqlx_core::query_scalar::query_scalar("SELECT user_id FROM accounts WHERE account_id = $1")
            .bind(account_id)
            .fetch_one(&pool)
            .await
            .expect("user id");

    let joined = format!("!joined-{}:localhost", Uuid::new_v4());
    let left = format!("!left-{}:localhost", Uuid::new_v4());
    let banned = format!("!banned-{}:localhost", Uuid::new_v4());
    let no_member = format!("!nomember-{}:localhost", Uuid::new_v4());

    for room in [&joined, &left, &banned, &no_member] {
        insert_message(&store, account_id, room, 1_000, "hi").await;
    }
    set_membership(&store, account_id, &user_id, &joined, "join").await;
    set_membership(&store, account_id, &user_id, &left, "leave").await;
    set_membership(&store, account_id, &user_id, &banned, "ban").await;
    // `no_member` gets no membership row.

    let rooms = store.list_rooms(Some(account_id)).await.expect("list");
    let ids: Vec<&str> = rooms.iter().map(|r| r.room_id.as_str()).collect();
    assert!(ids.contains(&joined.as_str()), "joined room shown");
    assert!(
        ids.contains(&no_member.as_str()),
        "missing membership row never hides a room"
    );
    assert!(!ids.contains(&left.as_str()), "left room hidden");
    assert!(!ids.contains(&banned.as_str()), "banned room hidden");

    common::cleanup_account(&pool, account_id).await;
}

/// `list_rooms` hides tombstoned (upgraded) rooms. When a room has an
/// `m.room.tombstone` state event it has been superseded by a replacement room
/// and should not appear in the list alongside its successor.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn list_rooms_excludes_tombstoned_rooms() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "tomb").await;

    let live = format!("!live-{}:localhost", Uuid::new_v4());
    let tombstoned = format!("!tomb-{}:localhost", Uuid::new_v4());
    let replacement = format!("!replacement-{}:localhost", Uuid::new_v4());

    insert_message(&store, account_id, &live, 1_000, "hi").await;
    insert_message(&store, account_id, &tombstoned, 1_000, "old").await;
    insert_message(&store, account_id, &replacement, 2_000, "new").await;

    // Mark `tombstoned` as upgraded — this is the state event Matrix sends when
    // a room is replaced by a newer room.
    set_state(
        &store,
        account_id,
        &tombstoned,
        "m.room.tombstone",
        json!({ "body": "This room has been replaced", "replacement_room": replacement }),
    )
    .await;

    let rooms = store.list_rooms(Some(account_id)).await.expect("list");
    let ids: Vec<&str> = rooms.iter().map(|r| r.room_id.as_str()).collect();
    assert!(ids.contains(&live.as_str()), "live room shown");
    assert!(
        ids.contains(&replacement.as_str()),
        "replacement room shown"
    );
    assert!(
        !ids.contains(&tombstoned.as_str()),
        "tombstoned room hidden"
    );

    common::cleanup_account(&pool, account_id).await;
}

/// `list_rooms` filters by account: a room under one account never leaks into
/// another's list, and `None` returns both accounts' rooms.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn list_rooms_filters_by_account() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let acc1 = test_account(&store, "f1").await;
    let acc2 = test_account(&store, "f2").await;
    let room1 = format!("!r1-{}:localhost", Uuid::new_v4());
    let room2 = format!("!r2-{}:localhost", Uuid::new_v4());
    insert_message(&store, acc1, &room1, 1_000, "one").await;
    insert_message(&store, acc2, &room2, 1_000, "two").await;

    let only1 = store.list_rooms(Some(acc1)).await.expect("list acc1");
    assert_eq!(only1.len(), 1);
    assert_eq!(only1[0].room_id, room1);
    assert_eq!(only1[0].account_id, acc1);
    assert!(only1[0].account_user_id.starts_with("@f1-"));

    // None spans accounts; both of ours appear (alongside any others in the DB).
    let all = store.list_rooms(None).await.expect("list all");
    assert!(all
        .iter()
        .any(|r| r.room_id == room1 && r.account_id == acc1));
    assert!(all
        .iter()
        .any(|r| r.room_id == room2 && r.account_id == acc2));

    common::cleanup_account(&pool, acc1).await;
    common::cleanup_account(&pool, acc2).await;
}

/// `list_rooms` excludes rooms belonging to a deactivated (logged-out) account,
/// mirroring `list_accounts`'s active-only filter, while sibling active
/// accounts' rooms are unaffected.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn list_rooms_excludes_deactivated_accounts() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let acc1 = test_account(&store, "g1").await;
    let acc2 = test_account(&store, "g2").await;
    let room1 = format!("!r1-{}:localhost", Uuid::new_v4());
    let room2 = format!("!r2-{}:localhost", Uuid::new_v4());
    insert_message(&store, acc1, &room1, 1_000, "one").await;
    insert_message(&store, acc2, &room2, 1_000, "two").await;

    store
        .set_account_state(acc1, AccountState::Deactivated)
        .await
        .expect("deactivate acc1");

    let all = store.list_rooms(None).await.expect("list all");
    assert!(!all.iter().any(|r| r.account_id == acc1));
    assert!(all
        .iter()
        .any(|r| r.room_id == room2 && r.account_id == acc2));

    // Scoping to the deactivated account explicitly also yields nothing.
    let only1 = store.list_rooms(Some(acc1)).await.expect("list acc1");
    assert!(only1.is_empty());

    common::cleanup_account(&pool, acc1).await;
    common::cleanup_account(&pool, acc2).await;
}

/// `get_event` returns a present event, `None` for a miss, and masks a redacted
/// event's content while reporting the redaction id.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn get_event_hit_miss_and_redaction_masking() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "ge").await;
    let room_id = format!("!ge-{}:localhost", Uuid::new_v4());

    // Hit: a plain message comes back with content and no redaction.
    let msg = insert_message(&store, account_id, &room_id, 1_000, "hello").await;
    let got = store
        .get_event(account_id, &msg)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(got.event_id, msg);
    assert_eq!(got.decrypted_body_text.as_deref(), Some("hello"));
    assert!(got.content.is_some());
    assert!(got.redaction_event_id.is_none());

    // Miss: an unknown id is None, not an error.
    assert!(store
        .get_event(account_id, "$nope:localhost")
        .await
        .expect("get miss")
        .is_none());

    // Redaction masking: the redacted message reads back masked.
    let redaction = insert_redaction(&store, account_id, &room_id, 2_000, &msg).await;
    let masked = store
        .get_event(account_id, &msg)
        .await
        .expect("get redacted")
        .expect("present");
    assert!(masked.content.is_none(), "content masked at read time");
    assert!(masked.decrypted_body_text.is_none(), "body masked");
    assert_eq!(
        masked.redaction_event_id.as_deref(),
        Some(redaction.as_str())
    );

    common::cleanup_account(&pool, account_id).await;
}
