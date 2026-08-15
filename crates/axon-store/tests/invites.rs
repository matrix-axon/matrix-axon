//! Integration tests for the pending-invite projection (`list_invites`).
//!
//! Like the other store integration tests these need a running Postgres and
//! are `#[ignore]`d by default:
//!
//! ```sh
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store -- --ignored
//! ```

mod common;

use axon_store::{AccountState, RoomInviteSnapshot};
use common::test_account;
use uuid::Uuid;

fn snapshot(inviter: &str, name: &str) -> RoomInviteSnapshot {
    RoomInviteSnapshot {
        name: Some(name.to_owned()),
        avatar_url: Some("mxc://hs/avatar".to_owned()),
        topic: Some("topic".to_owned()),
        canonical_alias: Some("#alias:localhost".to_owned()),
        room_type: None,
        inviter_user_id: inviter.to_owned(),
        inviter_display_name: Some("Inviter".to_owned()),
        is_direct: false,
        encrypted: true,
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn list_invites_orders_newest_first_and_joins_account_user() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "invites").await;
    let older = format!("!old-{}:localhost", Uuid::new_v4());
    let newer = format!("!new-{}:localhost", Uuid::new_v4());

    store
        .upsert_room_invite(account_id, &older, &snapshot("@a:localhost", "Older"))
        .await
        .expect("older");
    store
        .upsert_room_invite(account_id, &newer, &snapshot("@b:localhost", "Newer"))
        .await
        .expect("newer");
    // invited_at is DEFAULT now(); pin the two rows so order is deterministic.
    sqlx_core::query::query(
        "UPDATE room_invites SET invited_at = TIMESTAMPTZ '2020-01-01 00:00:00+00' \
         WHERE account_id = $1 AND room_id = $2",
    )
    .bind(account_id)
    .bind(&older)
    .execute(&pool)
    .await
    .expect("backdate older");
    sqlx_core::query::query(
        "UPDATE room_invites SET invited_at = TIMESTAMPTZ '2020-01-02 00:00:00+00' \
         WHERE account_id = $1 AND room_id = $2",
    )
    .bind(account_id)
    .bind(&newer)
    .execute(&pool)
    .await
    .expect("backdate newer");

    let invites = store.list_invites(Some(account_id)).await.expect("list");
    assert_eq!(
        invites
            .iter()
            .map(|i| i.room_id.as_str())
            .collect::<Vec<_>>(),
        vec![newer.as_str(), older.as_str()]
    );
    assert_eq!(invites[0].name.as_deref(), Some("Newer"));
    assert_eq!(invites[0].inviter_user_id, "@b:localhost");
    assert!(invites[0].encrypted);
    assert!(!invites[0].account_user_id.is_empty());

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn upsert_preserves_invited_at_and_refreshes_display_fields() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "invite-upsert").await;
    let room_id = format!("!r-{}:localhost", Uuid::new_v4());

    let first = store
        .upsert_room_invite(account_id, &room_id, &snapshot("@a:localhost", "Before"))
        .await
        .expect("insert");
    let mut updated = snapshot("@a:localhost", "After");
    updated.inviter_display_name = Some("Ada".to_owned());
    let second = store
        .upsert_room_invite(account_id, &room_id, &updated)
        .await
        .expect("update");
    assert_eq!(first, second, "invited_at must not bump on refresh");

    let invites = store.list_invites(Some(account_id)).await.expect("list");
    assert_eq!(invites.len(), 1);
    assert_eq!(invites[0].name.as_deref(), Some("After"));
    assert_eq!(invites[0].inviter_display_name.as_deref(), Some("Ada"));
    assert_eq!(invites[0].invited_at, first);

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn list_invites_excludes_deactivated_accounts_and_other_accounts() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let acc1 = test_account(&store, "invite-a").await;
    let acc2 = test_account(&store, "invite-b").await;
    let room = format!("!r-{}:localhost", Uuid::new_v4());

    store
        .upsert_room_invite(acc1, &room, &snapshot("@a:localhost", "A"))
        .await
        .expect("acc1");
    store
        .upsert_room_invite(acc2, &room, &snapshot("@a:localhost", "B"))
        .await
        .expect("acc2");

    let only1 = store.list_invites(Some(acc1)).await.expect("acc1 list");
    assert_eq!(only1.len(), 1);
    assert_eq!(only1[0].account_id, acc1);

    store
        .set_account_state(acc1, AccountState::Deactivated)
        .await
        .expect("deactivate");
    let after = store.list_invites(None).await.expect("all");
    assert!(
        after.iter().all(|i| i.account_id != acc1),
        "deactivated account's invites must not appear"
    );
    assert!(after.iter().any(|i| i.account_id == acc2));

    common::cleanup_account(&pool, acc1).await;
    common::cleanup_account(&pool, acc2).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn delete_reports_whether_a_row_was_removed() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "invite-del").await;
    let keep = format!("!keep-{}:localhost", Uuid::new_v4());
    let drop = format!("!drop-{}:localhost", Uuid::new_v4());

    store
        .upsert_room_invite(account_id, &keep, &snapshot("@a:localhost", "Keep"))
        .await
        .expect("keep");
    store
        .upsert_room_invite(account_id, &drop, &snapshot("@a:localhost", "Drop"))
        .await
        .expect("drop");

    assert!(store
        .delete_room_invite(account_id, &drop)
        .await
        .expect("delete one"));
    assert!(!store
        .delete_room_invite(account_id, &drop)
        .await
        .expect("already gone"));

    let left = store.list_invites(Some(account_id)).await.expect("left");
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].room_id, keep);

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn snapshots_round_trip_for_watcher_seed() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "invite-seed").await;
    let room_id = format!("!r-{}:localhost", Uuid::new_v4());
    let snap = snapshot("@a:localhost", "Seed");
    store
        .upsert_room_invite(account_id, &room_id, &snap)
        .await
        .expect("insert");

    let seeded = store.room_invite_snapshots(account_id).await.expect("seed");
    assert_eq!(seeded.len(), 1);
    assert_eq!(seeded[0].0, room_id);
    assert_eq!(seeded[0].1, snap);

    common::cleanup_account(&pool, account_id).await;
}
