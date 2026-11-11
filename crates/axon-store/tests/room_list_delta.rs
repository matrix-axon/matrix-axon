//! Integration tests for incremental room-list reads (`read_room_list`,
//! ADR 0134): which writes put a room in a delta, which do not, and how
//! removals and the listed count are reported.
//!
//! Like the other store integration tests these need a running Postgres and are
//! `#[ignore]`d by default. Run them with:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store -- --ignored
//! ```

mod common;

use axon_store::{AccountDataUpsert, AccountState, RoomListRead, RoomStateUpsert, Store};
use chrono::{DateTime, Utc};
use common::{insert_message, test_account};
use serde_json::{json, Value};
use sqlx_postgres::PgPool;
use uuid::Uuid;

fn room(prefix: &str) -> String {
    format!("!{prefix}-{}:localhost", Uuid::new_v4())
}

/// Move every change stamp the delta reads for `account_id` a day into the
/// past, well behind any cursor's overlap, so the next delta holds only what a
/// test writes after this.
///
/// The unread and tag rows keep their `updated_at` up to date with a trigger on
/// UPDATE, so they are deleted and re-inserted with an explicit time instead.
/// Deleting the unread rows bumps their summaries (ADR 0134), so the summaries
/// are backdated last.
async fn backdate(pool: &PgPool, account_id: Uuid) {
    let mut tx = pool.begin().await.expect("begin");
    let unread: Vec<(String, i64, i64)> = sqlx_core::query_as::query_as(
        "DELETE FROM room_unread_counts WHERE account_id = $1 \
         RETURNING room_id, notification_count, highlight_count",
    )
    .bind(account_id)
    .fetch_all(&mut *tx)
    .await
    .expect("take unread");
    for (room_id, notification_count, highlight_count) in unread {
        sqlx_core::query::query(
            "INSERT INTO room_unread_counts \
               (account_id, room_id, notification_count, highlight_count, updated_at) \
             VALUES ($1, $2, $3, $4, now() - interval '1 day')",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(notification_count)
        .bind(highlight_count)
        .execute(&mut *tx)
        .await
        .expect("restore unread");
    }
    let tags: Vec<(String, Value)> = sqlx_core::query_as::query_as(
        "DELETE FROM account_data WHERE account_id = $1 AND event_type = 'm.tag' \
         RETURNING room_id, content",
    )
    .bind(account_id)
    .fetch_all(&mut *tx)
    .await
    .expect("take tags");
    for (room_id, content) in tags {
        sqlx_core::query::query(
            "INSERT INTO account_data (account_id, room_id, event_type, content, updated_at) \
             VALUES ($1, $2, 'm.tag', $3, now() - interval '1 day')",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(content)
        .execute(&mut *tx)
        .await
        .expect("restore tag");
    }
    sqlx_core::query::query(
        "UPDATE room_summaries SET list_changed_at = now() - interval '1 day' \
         WHERE account_id = $1",
    )
    .bind(account_id)
    .execute(&mut *tx)
    .await
    .expect("backdate summaries");
    sqlx_core::query::query(
        "UPDATE accounts SET list_changed_at = now() - interval '1 day' \
         WHERE account_id = $1",
    )
    .bind(account_id)
    .execute(&mut *tx)
    .await
    .expect("backdate account");
    tx.commit().await.expect("commit");
}

/// Backdate, then take a full read whose cursor the test's writes come after.
async fn settled_cursor(store: &Store, pool: &PgPool, account_id: Uuid) -> DateTime<Utc> {
    backdate(pool, account_id).await;
    let read = store
        .read_room_list(Some(account_id), None)
        .await
        .expect("full read");
    assert!(read.full);
    read.as_of
}

async fn delta(store: &Store, account_id: Uuid, since: DateTime<Utc>) -> RoomListRead {
    let read = store
        .read_room_list(Some(account_id), Some(since))
        .await
        .expect("delta read");
    assert!(
        !read.full,
        "a cursor from this clock is answered as a delta"
    );
    read
}

fn changed_ids(read: &RoomListRead) -> Vec<&str> {
    let mut ids: Vec<&str> = read.rooms.iter().map(|r| r.room_id.as_str()).collect();
    ids.sort_unstable();
    ids
}

fn removed_ids(read: &RoomListRead) -> Vec<&str> {
    let mut ids: Vec<&str> = read.removed.iter().map(|(_, id)| id.as_str()).collect();
    ids.sort_unstable();
    ids
}

fn sorted(mut ids: Vec<&str>) -> Vec<&str> {
    ids.sort_unstable();
    ids
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
            origin_ts: 1_000 + i64::from(rand_ts()),
            content: Some(content),
        })
        .await
        .expect("set state");
}

/// A distinct, increasing origin_ts per call so state upserts are never
/// rejected as older than what is stored.
fn rand_ts() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

async fn set_direct(store: &Store, account_id: Uuid, content: Value) {
    store
        .upsert_account_data(&AccountDataUpsert {
            account_id,
            room_id: None,
            event_type: "m.direct",
            content,
        })
        .await
        .expect("m.direct");
}

async fn set_membership(
    store: &Store,
    account_id: Uuid,
    user_id: &str,
    room_id: &str,
    membership: &str,
    origin_ts: i64,
) {
    store
        .upsert_room_state_for_local_user(
            &RoomStateUpsert {
                account_id,
                room_id,
                event_type: "m.room.member",
                state_key: user_id,
                event_id: &format!("$mem-{}:localhost", Uuid::new_v4()),
                sender: user_id,
                origin_ts,
                content: Some(json!({ "membership": membership })),
            },
            Some(user_id),
        )
        .await
        .expect("membership");
}

/// A full read lists every room and names nothing removed; its count is the
/// list's length, and a delta right after it reports the same count.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn full_read_lists_every_room_with_its_count() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "rl-full").await;
    let (a, b) = (room("a"), room("b"));
    insert_message(&store, account_id, &a, 1_000, "a").await;
    insert_message(&store, account_id, &b, 2_000, "b").await;

    let read = store
        .read_room_list(Some(account_id), None)
        .await
        .expect("full read");
    assert!(read.full);
    assert_eq!(
        read.rooms
            .iter()
            .map(|r| r.room_id.as_str())
            .collect::<Vec<_>>(),
        [b.as_str(), a.as_str()],
        "newest activity first, as list_rooms"
    );
    assert!(read.removed.is_empty());
    assert_eq!(read.total, 2);

    backdate(&pool, account_id).await;
    let quiet = delta(&store, account_id, read.as_of).await;
    assert!(quiet.rooms.is_empty() && quiet.removed.is_empty());
    assert_eq!(quiet.total, 2, "a delta still counts the whole list");

    common::cleanup_account(&pool, account_id).await;
}

/// New activity and a changed name each put the room in the delta, with its
/// current values; untouched rooms stay out.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn activity_and_display_changes_reach_the_delta() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "rl-change").await;
    let (busy, renamed, quiet) = (room("busy"), room("renamed"), room("quiet"));
    for r in [&busy, &renamed, &quiet] {
        insert_message(&store, account_id, r, 1_000, "hi").await;
    }
    let since = settled_cursor(&store, &pool, account_id).await;

    let event_id = insert_message(&store, account_id, &busy, 5_000, "new").await;
    set_state(
        &store,
        account_id,
        &renamed,
        "m.room.name",
        json!({ "name": "Renamed" }),
    )
    .await;

    let read = delta(&store, account_id, since).await;
    assert_eq!(
        changed_ids(&read),
        sorted(vec![busy.as_str(), renamed.as_str()])
    );
    let busy_row = read.rooms.iter().find(|r| r.room_id == busy).unwrap();
    assert_eq!(busy_row.last_activity_ts, 5_000);
    assert_eq!(busy_row.last_event_id.as_deref(), Some(event_id.as_str()));
    let renamed_row = read.rooms.iter().find(|r| r.room_id == renamed).unwrap();
    assert_eq!(renamed_row.name.as_deref(), Some("Renamed"));
    assert_eq!(read.total, 3);

    common::cleanup_account(&pool, account_id).await;
}

/// The writes that rewrite a summary row without changing what the list shows
/// stay out of the delta. Every room-state write rewrites the display columns
/// through `refresh_room_summary_display`, which after a restart touched every
/// row in production; member counts are not in the list (ADR 0134).
#[tokio::test]
#[ignore = "requires Postgres"]
async fn rewrites_the_list_does_not_show_stay_out_of_the_delta() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "rl-noop").await;
    let r = room("noop");
    insert_message(&store, account_id, &r, 1_000, "hi").await;
    set_state(
        &store,
        account_id,
        &r,
        "m.room.name",
        json!({ "name": "Same" }),
    )
    .await;
    let since = settled_cursor(&store, &pool, account_id).await;

    sqlx_core::query::query("SELECT refresh_room_summary_display($1, $2)")
        .bind(account_id)
        .bind(&r)
        .execute(&pool)
        .await
        .expect("display refresh");
    sqlx_core::query::query(
        "UPDATE room_summaries \
         SET joined_member_count = 7, invited_member_count = 1, member_counts_observed_at = 9 \
         WHERE account_id = $1 AND room_id = $2",
    )
    .bind(account_id)
    .bind(&r)
    .execute(&pool)
    .await
    .expect("member counts");
    let updated_moved: bool = sqlx_core::query_scalar::query_scalar(
        "SELECT updated_at > list_changed_at FROM room_summaries \
         WHERE account_id = $1 AND room_id = $2",
    )
    .bind(account_id)
    .bind(&r)
    .fetch_one(&pool)
    .await
    .expect("stamps");
    assert!(updated_moved, "control: the row was rewritten");

    let read = delta(&store, account_id, since).await;
    assert!(read.rooms.is_empty(), "{:?}", changed_ids(&read));

    common::cleanup_account(&pool, account_id).await;
}

/// Leaving a room reports it in `removed`, not `rooms`, and the count drops.
/// Rejoining brings it back as a changed room.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_left_room_is_removed_and_a_rejoined_one_returns() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "rl-leave").await;
    let user_id = store
        .get_account(account_id)
        .await
        .unwrap()
        .unwrap()
        .user_id;
    let (stay, leave) = (room("stay"), room("leave"));
    insert_message(&store, account_id, &stay, 1_000, "hi").await;
    insert_message(&store, account_id, &leave, 1_000, "hi").await;
    let since = settled_cursor(&store, &pool, account_id).await;

    set_membership(&store, account_id, &user_id, &leave, "leave", 2_000).await;

    let read = delta(&store, account_id, since).await;
    assert!(read.rooms.is_empty());
    assert_eq!(removed_ids(&read), [leave.as_str()]);
    assert_eq!(read.removed[0].0, account_id);
    assert_eq!(read.total, 1);

    let since = settled_cursor(&store, &pool, account_id).await;
    set_membership(&store, account_id, &user_id, &leave, "join", 3_000).await;
    let read = delta(&store, account_id, since).await;
    assert_eq!(changed_ids(&read), [leave.as_str()]);
    assert!(read.removed.is_empty());
    assert_eq!(read.total, 2);

    common::cleanup_account(&pool, account_id).await;
}

/// Unread counts and tags live in their own rows; a write to either puts the
/// room in the delta, and so does deleting the unread row, which the list then
/// shows as zero.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn unread_and_tag_changes_reach_the_delta() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "rl-unread").await;
    let (unread, tagged, stale, quiet) =
        (room("unread"), room("tagged"), room("stale"), room("quiet"));
    for r in [&unread, &tagged, &stale, &quiet] {
        insert_message(&store, account_id, r, 1_000, "hi").await;
    }
    store
        .upsert_room_unread_counts(account_id, &stale, 4, 1)
        .await
        .unwrap();
    store
        .upsert_room_unread_counts(account_id, &quiet, 2, 0)
        .await
        .unwrap();
    let since = settled_cursor(&store, &pool, account_id).await;

    store
        .upsert_room_unread_counts(account_id, &unread, 3, 1)
        .await
        .unwrap();
    store
        .apply_room_tag(account_id, &tagged, "m.favourite", Some(0.5))
        .await
        .unwrap();
    // `stale` is no longer joined as far as the sync engine knows.
    store
        .delete_stale_room_unread_counts(
            account_id,
            &[unread.clone(), tagged.clone(), quiet.clone()],
        )
        .await
        .unwrap();

    let read = delta(&store, account_id, since).await;
    assert_eq!(
        changed_ids(&read),
        sorted(vec![unread.as_str(), tagged.as_str(), stale.as_str()])
    );
    let by_id = |id: &str| read.rooms.iter().find(|r| r.room_id == id).unwrap();
    assert_eq!(by_id(&unread).notification_count, 3);
    assert_eq!(by_id(&tagged).tags.len(), 1);
    assert_eq!(by_id(&stale).notification_count, 0);
    assert_eq!(by_id(&stale).highlight_count, 0);

    common::cleanup_account(&pool, account_id).await;
}

/// Deleting a tag row puts the room in the delta with no tags. Axon never
/// deletes one today; this guards a future or manual delete.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_deleted_tag_row_reaches_the_delta() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "rl-untag").await;
    let tagged = room("tagged");
    insert_message(&store, account_id, &tagged, 1_000, "hi").await;
    store
        .apply_room_tag(account_id, &tagged, "m.favourite", Some(0.5))
        .await
        .unwrap();
    let since = settled_cursor(&store, &pool, account_id).await;

    sqlx_core::query::query(
        "DELETE FROM account_data \
         WHERE account_id = $1 AND room_id = $2 AND event_type = 'm.tag'",
    )
    .bind(account_id)
    .bind(&tagged)
    .execute(&pool)
    .await
    .expect("delete tag row");

    let read = delta(&store, account_id, since).await;
    assert_eq!(changed_ids(&read), [tagged.as_str()]);
    assert!(read.rooms[0].tags.is_empty());

    common::cleanup_account(&pool, account_id).await;
}

/// A room left long ago is not reported as removed again when its unread
/// counts, tags or DM status change, or when its unread or tag row is
/// deleted: only what can take a room out of the list reports it.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn writes_to_a_left_room_do_not_report_it_removed_again() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "rl-left-churn").await;
    let user_id = store
        .get_account(account_id)
        .await
        .unwrap()
        .unwrap()
        .user_id;
    let (stay, left) = (room("stay"), room("left"));
    insert_message(&store, account_id, &stay, 1_000, "hi").await;
    insert_message(&store, account_id, &left, 1_000, "hi").await;
    set_membership(&store, account_id, &user_id, &left, "leave", 2_000).await;
    let since = settled_cursor(&store, &pool, account_id).await;

    store
        .upsert_room_unread_counts(account_id, &left, 5, 1)
        .await
        .unwrap();
    store
        .apply_room_tag(account_id, &left, "m.favourite", Some(0.5))
        .await
        .unwrap();
    set_direct(
        &store,
        account_id,
        json!({ "@bob:localhost": [left.clone()] }),
    )
    .await;
    let read = delta(&store, account_id, since).await;
    assert!(read.rooms.is_empty(), "{:?}", changed_ids(&read));
    assert!(read.removed.is_empty(), "{:?}", removed_ids(&read));

    store
        .delete_stale_room_unread_counts(account_id, std::slice::from_ref(&stay))
        .await
        .unwrap();
    sqlx_core::query::query(
        "DELETE FROM account_data \
         WHERE account_id = $1 AND room_id = $2 AND event_type = 'm.tag'",
    )
    .bind(account_id)
    .bind(&left)
    .execute(&pool)
    .await
    .expect("delete tag row");
    let read = delta(&store, account_id, since).await;
    assert!(read.rooms.is_empty() && read.removed.is_empty());
    assert_eq!(read.total, 1);

    common::cleanup_account(&pool, account_id).await;
}

/// `m.direct` is one value for the account; a write bumps exactly the rooms
/// whose membership in the map changed, carrying the new `is_direct`.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn an_m_direct_write_bumps_exactly_the_rooms_it_flips() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "rl-direct").await;
    let other = test_account(&store, "rl-direct-other").await;
    let (kept, added, dropped, plain) =
        (room("kept"), room("added"), room("dropped"), room("plain"));
    for r in [&kept, &added, &dropped, &plain] {
        insert_message(&store, account_id, r, 1_000, "hi").await;
    }
    insert_message(&store, other, &added, 1_000, "hi").await;
    set_direct(
        &store,
        account_id,
        json!({ "@bob:localhost": [kept, dropped] }),
    )
    .await;
    backdate(&pool, other).await;
    let since = settled_cursor(&store, &pool, account_id).await;

    // The same ids under another user and in another order flip nothing; the
    // non-string and non-array entries are ignored as the Rust reader does.
    set_direct(
        &store,
        account_id,
        json!({ "@carol:localhost": [dropped, 7, null], "@bob:localhost": [kept], "@x:localhost": "nope" }),
    )
    .await;
    let read = delta(&store, account_id, since).await;
    assert!(read.rooms.is_empty(), "{:?}", changed_ids(&read));

    set_direct(
        &store,
        account_id,
        json!({ "@bob:localhost": [kept, added] }),
    )
    .await;
    let read = delta(&store, account_id, since).await;
    assert_eq!(
        changed_ids(&read),
        sorted(vec![added.as_str(), dropped.as_str()])
    );
    let by_id = |id: &str| read.rooms.iter().find(|r| r.room_id == id).unwrap();
    assert!(by_id(&added).is_direct);
    assert!(!by_id(&dropped).is_direct);

    let other_read = store
        .read_room_list(Some(other), Some(since))
        .await
        .unwrap();
    assert!(
        other_read.rooms.is_empty(),
        "another account's copy of the room is not bumped"
    );

    common::cleanup_account(&pool, account_id).await;
    common::cleanup_account(&pool, other).await;
}

/// A room deleted outright leaves no row to report, so the delta names
/// nothing; `total` is what changes. A rebuilt row comes back with a fresh
/// stamp.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn deleted_rows_show_only_in_the_count() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "rl-gone").await;
    let (kept, purged) = (room("kept"), room("purged"));
    insert_message(&store, account_id, &kept, 1_000, "hi").await;
    insert_message(&store, account_id, &purged, 1_000, "hi").await;
    let since = settled_cursor(&store, &pool, account_id).await;

    sqlx_core::query::query("DELETE FROM room_summaries WHERE account_id = $1 AND room_id = $2")
        .bind(account_id)
        .bind(&purged)
        .execute(&pool)
        .await
        .expect("purge summary");
    let read = delta(&store, account_id, since).await;
    assert!(read.rooms.is_empty() && read.removed.is_empty());
    assert_eq!(read.total, 1, "the count is how a client notices");

    // A rebuild re-inserts every row from `events`, so both come back stamped.
    store
        .rebuild_room_summaries(account_id)
        .await
        .expect("rebuild");
    let read = delta(&store, account_id, since).await;
    assert_eq!(
        changed_ids(&read),
        sorted(vec![kept.as_str(), purged.as_str()])
    );
    assert_eq!(read.total, 2);

    common::cleanup_account(&pool, account_id).await;
}

/// An account leaving `active` reports its rooms as removed, and one returning
/// reports them as rooms, though neither touches a summary row. The count
/// cannot catch this on its own: here one account's room leaves as another's
/// returns, and `total` stays 1.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn account_activation_and_deactivation_reach_the_delta() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let (was_on, was_off) = (
        test_account(&store, "rl-on").await,
        test_account(&store, "rl-off").await,
    );
    let (on_room, off_room) = (room("on"), room("off"));
    insert_message(&store, was_on, &on_room, 1_000, "hi").await;
    insert_message(&store, was_off, &off_room, 1_000, "hi").await;
    store
        .set_account_state(was_off, AccountState::Deactivated)
        .await
        .unwrap();
    backdate(&pool, was_on).await;
    backdate(&pool, was_off).await;
    let scope = [was_on, was_off];
    let read = store.read_room_list(None, None).await.expect("full read");
    let listed = |read: &RoomListRead| -> Vec<String> {
        let mut ids: Vec<String> = read
            .rooms
            .iter()
            .filter(|r| scope.contains(&r.account_id))
            .map(|r| r.room_id.clone())
            .collect();
        ids.sort_unstable();
        ids
    };
    assert_eq!(listed(&read), vec![on_room.clone()]);
    let since = read.as_of;

    store
        .set_account_state(was_on, AccountState::Deactivated)
        .await
        .unwrap();
    store
        .set_account_state(was_off, AccountState::Active)
        .await
        .unwrap();
    let read = store
        .read_room_list(None, Some(since))
        .await
        .expect("delta read");
    assert!(!read.full);
    assert_eq!(listed(&read), vec![off_room.clone()]);
    assert!(read
        .removed
        .iter()
        .any(|(account, id)| *account == was_on && *id == on_room));
    assert!(!read.removed.iter().any(|(_, id)| *id == off_room));

    // A move between two states that are both unlisted stamps nothing.
    let since = settled_cursor(&store, &pool, was_on).await;
    backdate(&pool, was_off).await;
    store
        .set_account_state(was_on, AccountState::Deleting)
        .await
        .unwrap();
    let read = delta(&store, was_on, since).await;
    assert!(read.rooms.is_empty() && read.removed.is_empty());

    common::cleanup_account(&pool, was_on).await;
    common::cleanup_account(&pool, was_off).await;
}

/// A write stamped before a read's clock but committed after it is caught by
/// the overlap, and one stamped before the overlap is not (ADR 0134).
#[tokio::test]
#[ignore = "requires Postgres"]
async fn the_delta_reaches_back_by_the_overlap() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "rl-overlap").await;
    let overlap = store.room_list_overlap();
    assert_eq!(
        overlap,
        chrono::Duration::minutes(15),
        "default maintenance timeout → floor"
    );
    let (late, too_late) = (room("late"), room("too-late"));
    insert_message(&store, account_id, &late, 1_000, "hi").await;
    insert_message(&store, account_id, &too_late, 1_000, "hi").await;
    let since = settled_cursor(&store, &pool, account_id).await;

    for (r, at) in [
        (&late, since - overlap + chrono::Duration::seconds(5)),
        (&too_late, since - overlap - chrono::Duration::seconds(5)),
    ] {
        sqlx_core::query::query(
            "UPDATE room_summaries SET list_changed_at = $3 WHERE account_id = $1 AND room_id = $2",
        )
        .bind(account_id)
        .bind(r)
        .bind(at)
        .execute(&pool)
        .await
        .expect("stamp");
    }
    let read = delta(&store, account_id, since).await;
    assert_eq!(
        changed_ids(&read),
        [late.as_str()],
        "inside the overlap only"
    );

    common::cleanup_account(&pool, account_id).await;
}

/// A cursor later than the database clock did not come from it, so the read is
/// served in full rather than as a delta that could miss writes.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_cursor_from_the_future_reads_in_full() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "rl-future").await;
    let r = room("r");
    insert_message(&store, account_id, &r, 1_000, "hi").await;

    let read = store
        .read_room_list(
            Some(account_id),
            Some(Utc::now() + chrono::Duration::days(1)),
        )
        .await
        .expect("read");
    assert!(read.full);
    assert_eq!(changed_ids(&read), [r.as_str()]);
    assert_eq!(read.total, 1);

    common::cleanup_account(&pool, account_id).await;
}

/// The triggers must not get in the way of deleting an account, whose cascade
/// deletes its unread rows and `m.direct` while its summaries go too.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn deleting_an_account_still_cascades() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "rl-delete").await;
    let r = room("r");
    insert_message(&store, account_id, &r, 1_000, "hi").await;
    store
        .upsert_room_unread_counts(account_id, &r, 1, 0)
        .await
        .unwrap();
    set_direct(&store, account_id, json!({ "@bob:localhost": [r] })).await;

    store
        .delete_account_row(account_id)
        .await
        .expect("delete account");
    let left: i64 = sqlx_core::query_scalar::query_scalar(
        "SELECT count(*) FROM room_summaries WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 0);
}
