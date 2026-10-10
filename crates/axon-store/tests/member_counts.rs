//! Persistent summary observations and lifecycle invalidation.
mod common;
use axon_store::{MemberCountWrite, RoomMemberCounts, RoomStateUpsert};
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
        observed_at: chrono::Utc::now().timestamp_millis() - 10000,
    };
    assert_eq!(store.room_member_counts(account, room).await.unwrap(), None);
    store
        .set_room_member_counts(account, room, counts.clone())
        .await
        .unwrap();
    assert_eq!(store.room_member_counts(other, room).await.unwrap(), None);
    let version: (String,) = sqlx_core::query_as::query_as(
        "SELECT xmin::text FROM room_summaries WHERE account_id = $1 AND room_id = $2",
    )
    .bind(account)
    .bind(room)
    .fetch_one(store.pool())
    .await
    .unwrap();
    let identical = RoomMemberCounts {
        observed_at: counts.observed_at + 10,
        ..counts.clone()
    };
    assert_eq!(
        store
            .set_room_member_counts(account, room, identical)
            .await
            .unwrap(),
        MemberCountWrite::Unchanged
    );
    let after: (String,) = sqlx_core::query_as::query_as(
        "SELECT xmin::text FROM room_summaries WHERE account_id = $1 AND room_id = $2",
    )
    .bind(account)
    .bind(room)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(
        version, after,
        "identical observations must not rewrite the wide summary row"
    );
    assert_eq!(
        store.room_member_counts(account, room).await.unwrap(),
        Some(counts.clone()),
        "observed_at belongs to the changed pair, not subsequent confirmations"
    );
    let stale = RoomMemberCounts {
        joined: 499,
        observed_at: counts.observed_at - 1,
        ..counts.clone()
    };
    assert_eq!(
        store
            .set_room_member_counts(account, room, stale)
            .await
            .unwrap(),
        MemberCountWrite::Superseded
    );
    assert_eq!(
        store
            .invalidate_room_member_counts(account, room, counts.observed_at - 1)
            .await
            .unwrap(),
        MemberCountWrite::Superseded
    );
    assert_eq!(
        store
            .set_room_member_counts(account, "!missing:localhost", counts.clone())
            .await
            .unwrap(),
        MemberCountWrite::Retry
    );
    // Positive invalidation wins a millisecond tie with a count observation.
    assert_eq!(
        store
            .invalidate_room_member_counts(account, room, counts.observed_at)
            .await
            .unwrap(),
        MemberCountWrite::Applied
    );
    assert!(store
        .room_member_counts(account, room)
        .await
        .unwrap()
        .is_none());
    let counts = RoomMemberCounts {
        observed_at: counts.observed_at + 1,
        ..counts
    };
    store
        .set_room_member_counts(account, room, counts.clone())
        .await
        .unwrap();
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
        store.set_room_member_counts(
            account,
            room,
            RoomMemberCounts {
                joined: 501,
                observed_at: counts.observed_at + 1,
                ..counts.clone()
            },
        ),
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
                .set_room_member_counts(
                    account,
                    room,
                    RoomMemberCounts {
                        observed_at: chrono::Utc::now().timestamp_millis() + 1000,
                        ..counts.clone()
                    },
                )
                .await
                .unwrap();
            assert_eq!(store.room_member_counts(account, room).await.unwrap(), None);
        }
        if event == "$joined" {
            store
                .set_room_member_counts(
                    account,
                    room,
                    RoomMemberCounts {
                        observed_at: chrono::Utc::now().timestamp_millis() + 1000,
                        ..counts.clone()
                    },
                )
                .await
                .unwrap();
        }
    }
    // Rejoin only becomes available after a new observation; known zero invites.
    let next = RoomMemberCounts {
        joined: 499,
        invited: 0,
        observed_at: chrono::Utc::now().timestamp_millis() + 2000,
    };
    store
        .set_room_member_counts(account, room, next.clone())
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
        .set_room_member_counts(account, room, counts.clone())
        .await
        .unwrap();
    assert!(store
        .state_reconciliation_rooms(account, "")
        .await
        .unwrap()
        .is_empty());
    store.delete_account_row(account).await.unwrap();
    store
        .set_room_member_counts(account, room, counts)
        .await
        .unwrap();
    common::cleanup_account(store.pool(), other).await;
}
