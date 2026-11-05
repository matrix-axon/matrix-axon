//! The push badge total is the room list's unread sum (ADR 0129).
//!
//! `unread_badge_total` sums the whole database, so this binary holds one test
//! and compares it with `list_rooms(None)` read straight after. Cargo runs test
//! binaries one at a time, so no other store test writes counts in between.
//!
//! ```sh
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon \
//!   cargo test -p axon-store --test unread_badge_total -- --ignored
//! ```

mod common;

use axon_store::{AccountState, RoomStateUpsert, Store};
use common::{insert_message, test_account};
use serde_json::json;
use uuid::Uuid;

async fn listed_sum(store: &Store) -> u64 {
    store
        .list_rooms(None)
        .await
        .expect("list")
        .iter()
        .map(|room| u64::try_from(room.notification_count).expect("non-negative"))
        .sum()
}

/// Write one state event. `local_user` is the account's own user id for its
/// membership row, and `None` for any other state.
async fn set_state(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    event_type: &str,
    state_key: &str,
    content: serde_json::Value,
    local_user: Option<&str>,
) {
    store
        .upsert_room_state_for_local_user(
            &RoomStateUpsert {
                account_id,
                room_id,
                event_type,
                state_key,
                event_id: &format!("$state-{}:localhost", Uuid::new_v4()),
                sender: "@alice:localhost",
                origin_ts: 2_000,
                content: Some(content),
            },
            local_user,
        )
        .await
        .expect("set state");
}

/// Every room the list hides is also left out of the badge: a left room, a
/// tombstoned room, a deactivated account's room, and a count with no summary
/// row. Only the two listed rooms add to the total.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn the_badge_total_is_the_sum_of_the_listed_rooms() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let before = store.unread_badge_total().await.expect("total before");
    assert_eq!(before, listed_sum(&store).await, "baseline");

    let account = test_account(&store, "badge").await;
    let gone = test_account(&store, "badge-gone").await;
    let user_id: String =
        sqlx_core::query_scalar::query_scalar("SELECT user_id FROM accounts WHERE account_id = $1")
            .bind(account)
            .fetch_one(&pool)
            .await
            .expect("user id");

    let room = |label: &str| format!("!{label}-{}:localhost", Uuid::new_v4());
    let (listed, listed_too, left, tombstoned, unsummarized, deactivated) = (
        room("listed"),
        room("listed-too"),
        room("left"),
        room("tomb"),
        room("no-summary"),
        room("deactivated"),
    );
    for id in [&listed, &listed_too, &left, &tombstoned] {
        insert_message(&store, account, id, 1_000, "hi").await;
    }
    insert_message(&store, gone, &deactivated, 1_000, "hi").await;
    set_state(
        &store,
        account,
        &left,
        "m.room.member",
        &user_id,
        json!({ "membership": "leave" }),
        Some(&user_id),
    )
    .await;
    set_state(
        &store,
        account,
        &tombstoned,
        "m.room.tombstone",
        "",
        json!({ "body": "replaced", "replacement_room": listed }),
        None,
    )
    .await;

    for (account_id, room_id, count) in [
        (account, &listed, 4),
        (account, &listed_too, 3),
        (account, &left, 5),
        (account, &tombstoned, 7),
        (account, &unsummarized, 13),
        (gone, &deactivated, 17),
    ] {
        store
            .upsert_room_unread_counts(account_id, room_id, count, 0)
            .await
            .expect("unread counts");
    }
    store
        .set_account_state(gone, AccountState::Deactivated)
        .await
        .expect("deactivate");

    let after = store.unread_badge_total().await.expect("total after");
    assert_eq!(after, before + 4 + 3);
    assert_eq!(after, listed_sum(&store).await);

    common::cleanup_account(&pool, account).await;
    common::cleanup_account(&pool, gone).await;
}
