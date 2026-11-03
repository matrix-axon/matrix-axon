//! The room-detail acquisition cache (ADR 0126 decisions 2-4): eligibility,
//! membership-epoch fencing, and cleanup on leave, purge, and new state.
mod common;
use axon_store::{DetailKind, DetailOutcome, DetailWrite, RoomStateUpsert, Store};
use serde_json::json;
use uuid::Uuid;

const ROOM: &str = "!details:localhost";

async fn account(store: &Store) -> (Uuid, String) {
    let user = format!("@details-{}:localhost", Uuid::new_v4());
    let account = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .unwrap();
    (account.account_id, user)
}

async fn state(
    store: &Store,
    account_id: Uuid,
    user: &str,
    event_type: &str,
    key: &str,
    content: serde_json::Value,
) {
    let event_id = format!("$state-{}", Uuid::new_v4());
    store
        .upsert_room_state_for_local_user(
            &RoomStateUpsert {
                account_id,
                room_id: ROOM,
                event_type,
                state_key: key,
                event_id: &event_id,
                sender: user,
                origin_ts: chrono::Utc::now().timestamp_millis(),
                content: Some(content),
            },
            Some(user),
        )
        .await
        .unwrap();
}

async fn membership(store: &Store, account_id: Uuid, user: &str, membership: &str) {
    state(
        store,
        account_id,
        user,
        "m.room.member",
        user,
        json!({ "membership": membership }),
    )
    .await;
}

fn answer(status: &'static str, value: Option<serde_json::Value>) -> DetailOutcome {
    DetailOutcome::Answer {
        status,
        value,
        ttl_ms: 15 * 60 * 1000,
    }
}

fn failure(category: &'static str) -> DetailOutcome {
    DetailOutcome::Failure {
        category,
        retry_after_ms: None,
        min_backoff_ms: 60_000,
        max_backoff_ms: 1_800_000,
    }
}

async fn expire(store: &Store, account_id: Uuid) {
    sqlx_core::query::query(
        "UPDATE room_detail_acquisitions SET expires_at = expires_at - 86400000, \
         next_attempt_at = next_attempt_at - 86400000 WHERE account_id = $1",
    )
    .bind(account_id)
    .execute(store.pool())
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn only_joined_rooms_are_eligible_and_answers_report_real_changes() {
    let store = common::migrated_store().await;
    let (account_id, user) = account(&store).await;
    let kinds = [DetailKind::LocalAliases, DetailKind::DirectoryVisibility];

    // Unknown room, then a summary without the account's membership.
    assert!(
        !store
            .room_detail_snapshot(account_id, ROOM, &kinds)
            .await
            .unwrap()
            .eligible
    );
    common::insert_message(&store, account_id, ROOM, 1, "fixture").await;
    assert_eq!(store.joined_epoch(account_id, ROOM).await.unwrap(), None);
    membership(&store, account_id, &user, "invite").await;
    assert_eq!(store.joined_epoch(account_id, ROOM).await.unwrap(), None);
    membership(&store, account_id, &user, "join").await;
    let epoch = store.joined_epoch(account_id, ROOM).await.unwrap().unwrap();
    let snapshot = store
        .room_detail_snapshot(account_id, ROOM, &kinds)
        .await
        .unwrap();
    assert!(snapshot.eligible);
    assert!(snapshot.rows.is_empty());

    let aliases = answer("available", Some(json!(["#a:localhost"])));
    let record = |outcome: DetailOutcome| {
        let store = store.clone();
        async move {
            store
                .record_room_detail(
                    account_id,
                    ROOM,
                    epoch,
                    DetailKind::LocalAliases,
                    "",
                    &outcome,
                )
                .await
                .unwrap()
        }
    };
    assert_eq!(record(aliases.clone()).await, DetailWrite::Changed);
    assert_eq!(
        record(aliases.clone()).await,
        DetailWrite::Unchanged,
        "a fresh identical answer"
    );
    let row = &store
        .room_detail_snapshot(account_id, ROOM, &kinds)
        .await
        .unwrap()
        .rows[0];
    assert_eq!(row.kind, "local_aliases");
    assert_eq!(row.value_status.as_deref(), Some("available"));
    assert!(!row.due);
    assert!(row.expires_at.unwrap() > row.fetched_at.unwrap());

    // A stale value refreshed to the same answer is fresh again: a change.
    expire(&store, account_id).await;
    assert!(
        store
            .room_detail_snapshot(account_id, ROOM, &kinds)
            .await
            .unwrap()
            .rows[0]
            .due
    );
    assert_eq!(record(aliases.clone()).await, DetailWrite::Changed);

    // A failure keeps the stale value visible and backs off.
    expire(&store, account_id).await;
    assert_eq!(record(failure("timeout")).await, DetailWrite::Changed);
    let row = store
        .room_detail_snapshot(account_id, ROOM, &kinds)
        .await
        .unwrap()
        .rows
        .remove(0);
    assert_eq!(row.value, Some(json!(["#a:localhost"])));
    assert_eq!(row.failure.as_deref(), Some("timeout"));
    assert!(!row.due, "backoff is in force");
    let first_backoff = row.next_attempt_at.unwrap();
    assert_eq!(record(failure("timeout")).await, DetailWrite::Unchanged);
    let row = store
        .room_detail_snapshot(account_id, ROOM, &kinds)
        .await
        .unwrap()
        .rows
        .remove(0);
    assert!(
        row.next_attempt_at.unwrap() >= first_backoff + 60_000,
        "the backoff doubles"
    );
    // Retry-After wins over a shorter backoff.
    let long = DetailOutcome::Failure {
        category: "rate_limited",
        retry_after_ms: Some(3_600_000),
        min_backoff_ms: 60_000,
        max_backoff_ms: 1_800_000,
    };
    assert_eq!(record(long).await, DetailWrite::Changed);
    let row = store
        .room_detail_snapshot(account_id, ROOM, &kinds)
        .await
        .unwrap()
        .rows
        .remove(0);
    assert!(row.next_attempt_at.unwrap() >= row.fetched_at.unwrap() + 3_600_000);

    // A conclusive denial replaces the cached list.
    assert_eq!(
        record(answer("forbidden", None)).await,
        DetailWrite::Changed
    );
    let row = store
        .room_detail_snapshot(account_id, ROOM, &kinds)
        .await
        .unwrap()
        .rows
        .remove(0);
    assert_eq!(
        (row.value_status.as_deref(), row.value, row.failure),
        (Some("forbidden"), None, None)
    );

    common::cleanup_account(store.pool(), account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn leave_rejoin_purge_and_removal_fence_and_delete() {
    let store = common::migrated_store().await;
    let (account_id, user) = account(&store).await;
    common::insert_message(&store, account_id, ROOM, 1, "fixture").await;
    membership(&store, account_id, &user, "join").await;
    let before_leave = store.joined_epoch(account_id, ROOM).await.unwrap().unwrap();
    let directory = answer("available", Some(json!({ "visibility": "public" })));
    store
        .record_room_detail(
            account_id,
            ROOM,
            before_leave,
            DetailKind::DirectoryVisibility,
            "",
            &directory,
        )
        .await
        .unwrap();

    // Leave: rows go, and the epoch moves.
    membership(&store, account_id, &user, "leave").await;
    assert_eq!(store.joined_epoch(account_id, ROOM).await.unwrap(), None);
    let kinds = [DetailKind::DirectoryVisibility];
    let rows = |store: Store| async move {
        let (n,): (i64,) = sqlx_core::query_as::query_as(
            "SELECT count(*) FROM room_detail_acquisitions WHERE account_id = $1",
        )
        .bind(account_id)
        .fetch_one(store.pool())
        .await
        .unwrap();
        n
    };
    assert_eq!(rows(store.clone()).await, 0);
    assert!(
        !store
            .room_detail_snapshot(account_id, ROOM, &kinds)
            .await
            .unwrap()
            .eligible
    );

    // A job that started before the leave commits nothing, even after a rejoin.
    membership(&store, account_id, &user, "join").await;
    let rejoined = store.joined_epoch(account_id, ROOM).await.unwrap().unwrap();
    assert_ne!(rejoined, before_leave);
    assert_eq!(
        store
            .record_room_detail(
                account_id,
                ROOM,
                before_leave,
                DetailKind::DirectoryVisibility,
                "",
                &directory
            )
            .await
            .unwrap(),
        DetailWrite::Fenced
    );
    assert_eq!(rows(store.clone()).await, 0);

    // A purge rebuild re-creates the summary with a fresh epoch, never a reused one.
    store
        .record_room_detail(
            account_id,
            ROOM,
            rejoined,
            DetailKind::DirectoryVisibility,
            "",
            &directory,
        )
        .await
        .unwrap();
    store.rebuild_room_summaries(account_id).await.unwrap();
    assert_eq!(
        rows(store.clone()).await,
        0,
        "the summary foreign key cascades"
    );
    let rebuilt = store.joined_epoch(account_id, ROOM).await.unwrap().unwrap();
    assert!(rebuilt != rejoined && rebuilt != before_leave);
    assert_eq!(
        store
            .record_room_detail(
                account_id,
                ROOM,
                rejoined,
                DetailKind::DirectoryVisibility,
                "",
                &directory
            )
            .await
            .unwrap(),
        DetailWrite::Fenced
    );

    // Account removal fences and cascades.
    store
        .record_room_detail(
            account_id,
            ROOM,
            rebuilt,
            DetailKind::DirectoryVisibility,
            "",
            &directory,
        )
        .await
        .unwrap();
    common::cleanup_account(store.pool(), account_id).await;
    assert_eq!(rows(store.clone()).await, 0);
    assert_eq!(
        store
            .record_room_detail(
                account_id,
                ROOM,
                rebuilt,
                DetailKind::DirectoryVisibility,
                "",
                &directory
            )
            .await
            .unwrap(),
        DetailWrite::Fenced
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn absence_confirmations_yield_to_state() {
    let store = common::migrated_store().await;
    let (account_id, user) = account(&store).await;
    common::insert_message(&store, account_id, ROOM, 1, "fixture").await;
    membership(&store, account_id, &user, "join").await;
    state(
        &store,
        account_id,
        &user,
        "m.room.create",
        "",
        json!({ "room_version": "12" }),
    )
    .await;
    let epoch = store.joined_epoch(account_id, ROOM).await.unwrap().unwrap();
    let order = [
        "m.room.encryption",
        "m.room.server_acl",
        "m.room.create",
        "m.room.guest_access",
    ];

    // Types with a tuple are never due; the caller's order is kept.
    assert_eq!(
        store
            .due_absence_types(account_id, ROOM, &order)
            .await
            .unwrap(),
        [
            "m.room.encryption",
            "m.room.server_acl",
            "m.room.guest_access"
        ]
    );
    let absent = answer("absent", None);
    let confirm = |state_type: &'static str, outcome: DetailOutcome| {
        let store = store.clone();
        async move {
            store
                .record_room_detail(
                    account_id,
                    ROOM,
                    epoch,
                    DetailKind::Absence,
                    state_type,
                    &outcome,
                )
                .await
                .unwrap()
        }
    };
    assert_eq!(
        confirm("m.room.encryption", absent.clone()).await,
        DetailWrite::Changed
    );
    assert_eq!(
        confirm("m.room.encryption", absent.clone()).await,
        DetailWrite::Unchanged
    );
    assert_eq!(
        confirm("m.room.server_acl", failure("server_error")).await,
        DetailWrite::Changed
    );
    assert_eq!(
        store
            .due_absence_types(account_id, ROOM, &order)
            .await
            .unwrap(),
        ["m.room.guest_access"],
        "confirmed and backed-off types are not due"
    );
    // A 200 clears a confirmation and is not asked again for a while.
    assert_eq!(
        confirm("m.room.guest_access", absent.clone()).await,
        DetailWrite::Changed
    );
    assert_eq!(
        confirm(
            "m.room.guest_access",
            DetailOutcome::Present {
                retry_in_ms: 3_600_000
            }
        )
        .await,
        DetailWrite::Changed
    );
    assert!(store
        .due_absence_types(account_id, ROOM, &order)
        .await
        .unwrap()
        .is_empty());
    let snapshot = store
        .room_detail_snapshot(account_id, ROOM, &[DetailKind::Absence])
        .await
        .unwrap();
    let guest = snapshot
        .rows
        .iter()
        .find(|row| row.state_type == "m.room.guest_access")
        .unwrap();
    assert_eq!((guest.value_status.as_deref(), guest.due), (None, false));

    // Expired confirmations become due again.
    expire(&store, account_id).await;
    assert_eq!(
        store
            .due_absence_types(account_id, ROOM, &order)
            .await
            .unwrap(),
        [
            "m.room.encryption",
            "m.room.server_acl",
            "m.room.guest_access"
        ]
    );

    // State that arrives deletes the confirmation, and a confirmation that
    // races a tuple loses.
    state(
        &store,
        account_id,
        &user,
        "m.room.encryption",
        "",
        json!({ "algorithm": "m.megolm.v1.aes-sha2" }),
    )
    .await;
    let snapshot = store
        .room_detail_snapshot(account_id, ROOM, &[DetailKind::Absence])
        .await
        .unwrap();
    assert!(snapshot
        .rows
        .iter()
        .all(|row| row.state_type != "m.room.encryption"));
    assert_eq!(
        confirm("m.room.encryption", absent.clone()).await,
        DetailWrite::Fenced
    );

    common::cleanup_account(store.pool(), account_id).await;
}

/// A confirmation racing an uncommitted tuple of its type waits for it and
/// then loses, rather than committing beside it: neither side can see the
/// other's uncommitted row, so they serialize on an advisory lock.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_confirmation_cannot_commit_beside_a_concurrent_tuple() {
    let store = common::migrated_store().await;
    let (account_id, user) = account(&store).await;
    common::insert_message(&store, account_id, ROOM, 1, "fixture").await;
    membership(&store, account_id, &user, "join").await;
    let epoch = store.joined_epoch(account_id, ROOM).await.unwrap().unwrap();

    // The tuple's insert holds its transaction open, past its trigger.
    let pool = common::raw_pool().await;
    let mut tuple = pool.begin().await.unwrap();
    sqlx_core::query::query(
        "INSERT INTO room_state (account_id, room_id, event_type, state_key, event_id, sender, origin_ts, content) \
         VALUES ($1, $2, 'm.room.server_acl', '', '$acl', $3, 1, '{\"allow\": [\"*\"]}')",
    )
    .bind(account_id)
    .bind(ROOM)
    .bind(&user)
    .execute(&mut *tuple)
    .await
    .unwrap();
    let confirm = tokio::spawn({
        let store = store.clone();
        async move {
            store
                .record_room_detail(
                    account_id,
                    ROOM,
                    epoch,
                    DetailKind::Absence,
                    "m.room.server_acl",
                    &answer("absent", None),
                )
                .await
                .unwrap()
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !confirm.is_finished(),
        "the confirmation waits for the tuple"
    );
    tuple.commit().await.unwrap();
    assert_eq!(confirm.await.unwrap(), DetailWrite::Fenced);
    let (rows,): (i64,) = sqlx_core::query_as::query_as(
        "SELECT count(*) FROM room_detail_acquisitions WHERE account_id = $1 AND kind = 'absence'",
    )
    .bind(account_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(rows, 0);

    common::cleanup_account(store.pool(), account_id).await;
}
