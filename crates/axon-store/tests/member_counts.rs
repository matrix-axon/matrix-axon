//! Persistent summary observations and lifecycle invalidation.
mod common;
use axon_store::{RoomMemberCounts, RoomStateUpsert};
use serde_json::json;

#[tokio::test]
#[ignore = "requires Postgres"]
async fn member_counts_restart_transitions_and_account_isolation() {
    let store = common::migrated_store().await;
    let account = common::test_account(&store, "counts").await;
    let other = common::test_account(&store, "counts-other").await;
    let room = "!counts:localhost";
    for id in [account, other] {
        common::insert_message(&store, id, room, 1, "fixture").await;
    }
    let counts = RoomMemberCounts {
        joined: 500,
        invited: 3,
        observed_at: 1234,
    };
    assert_eq!(store.room_member_counts(account, room).await.unwrap(), None);
    store
        .set_room_member_counts(account, room, Some(counts.clone()))
        .await
        .unwrap();
    assert_eq!(store.room_member_counts(other, room).await.unwrap(), None);
    // A contended room cannot pin the worker's PostgreSQL connection forever.
    let mut lock = store.pool().begin().await.unwrap();
    sqlx_core::query::query(
        "UPDATE room_summaries SET name = name WHERE account_id = $1 AND room_id = $2",
    )
    .bind(account)
    .bind(room)
    .execute(&mut *lock)
    .await
    .unwrap();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        store.set_room_member_counts(account, room, Some(counts.clone())),
    )
    .await
    .unwrap();
    assert!(outcome.is_err());
    lock.rollback().await.unwrap();
    let reopened = common::migrated_store().await;
    assert_eq!(
        reopened.room_member_counts(account, room).await.unwrap(),
        Some(counts.clone())
    );
    // Leave invalidates atomically, including a late joined observation.
    for (event, membership) in [
        ("$left", "leave"),
        ("$joined", "join"),
        ("$banned", "ban"),
        ("$rejoined", "join"),
    ] {
        let user = store.get_account(account).await.unwrap().unwrap().user_id;
        store
            .upsert_room_state_for_local_user(
                &RoomStateUpsert {
                    account_id: account,
                    room_id: room,
                    event_type: "m.room.member",
                    state_key: &user,
                    event_id: event,
                    sender: &user,
                    origin_ts: 2,
                    content: Some(json!({"membership":membership})),
                },
                Some(&user),
            )
            .await
            .unwrap();
        assert_eq!(store.room_member_counts(account, room).await.unwrap(), None);
        if membership != "join" {
            store
                .set_room_member_counts(account, room, Some(counts.clone()))
                .await
                .unwrap();
            assert_eq!(store.room_member_counts(account, room).await.unwrap(), None);
        }
        if event == "$joined" {
            store
                .set_room_member_counts(account, room, Some(counts.clone()))
                .await
                .unwrap();
        }
    }
    // Rejoin only becomes available after a new observation; known zero invites.
    let next = RoomMemberCounts {
        joined: 499,
        invited: 0,
        observed_at: 5678,
    };
    store
        .set_room_member_counts(account, room, Some(next.clone()))
        .await
        .unwrap();
    assert_eq!(
        store.room_member_counts(account, room).await.unwrap(),
        Some(next)
    );
    store
        .flag_room_upstream_suspect(account, room, "fixture")
        .await
        .unwrap();
    store
        .mark_room_upstream_gone(account, room, "fixture")
        .await
        .unwrap();
    assert_eq!(store.room_member_counts(account, room).await.unwrap(), None);
    // A late worker cannot recreate a purged summary or deleted account.
    sqlx_core::query::query("DELETE FROM room_summaries WHERE account_id = $1 AND room_id = $2")
        .bind(account)
        .bind(room)
        .execute(store.pool())
        .await
        .unwrap();
    store
        .set_room_member_counts(account, room, Some(counts.clone()))
        .await
        .unwrap();
    assert!(store
        .state_reconciliation_rooms(account, "")
        .await
        .unwrap()
        .is_empty());
    store.delete_account_row(account).await.unwrap();
    store
        .set_room_member_counts(account, room, Some(counts))
        .await
        .unwrap();
    common::cleanup_account(store.pool(), other).await;
}
