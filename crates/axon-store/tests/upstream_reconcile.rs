//! Integration tests for the upstream-room reconcile state (ADR 0090).
//!
//! Like the other store integration tests these need a running Postgres and
//! are `#[ignore]`d by default. Run them with:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store -- --ignored
//! ```

mod common;

use common::test_account;
use uuid::Uuid;

/// The two-step lifecycle: a rejection flags `suspect`, a confirming probe
/// promotes it to `gone`, and only then does the room show up in the set the
/// unread watcher skips. A repeat rejection must not disturb an existing row —
/// neither by resetting how long the room has looked wrong, nor by demoting a
/// confirmed `gone` back to `suspect`.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn suspect_is_promoted_to_gone_and_never_demoted() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "reconcile-lifecycle").await;
    let room_id = format!("!purged-{}:localhost", Uuid::new_v4());

    store
        .flag_room_upstream_suspect(account_id, &room_id, "M_FORBIDDEN: user not in room")
        .await
        .expect("flag suspect");

    assert_eq!(
        store
            .suspect_upstream_rooms(account_id)
            .await
            .expect("suspects"),
        vec![room_id.clone()]
    );
    assert!(
        store
            .rooms_gone_upstream(account_id)
            .await
            .expect("gone")
            .is_empty(),
        "a suspect room is not yet acted on"
    );

    let flagged_at = first_flagged_at(&pool, account_id, &room_id).await;

    // A second rejection arrives before the probe runs.
    store
        .flag_room_upstream_suspect(account_id, &room_id, "M_FORBIDDEN: again")
        .await
        .expect("flag suspect again");
    assert_eq!(
        first_flagged_at(&pool, account_id, &room_id).await,
        flagged_at,
        "first_flagged_at survives a repeat rejection"
    );

    assert!(
        store
            .mark_room_upstream_gone(account_id, &room_id, "probe: M_FORBIDDEN")
            .await
            .expect("mark gone"),
        "promoting a room still under suspicion applies"
    );

    assert!(
        store
            .suspect_upstream_rooms(account_id)
            .await
            .expect("suspects")
            .is_empty(),
        "a confirmed room is no longer awaiting a probe"
    );
    assert!(store
        .rooms_gone_upstream(account_id)
        .await
        .expect("gone")
        .contains(&room_id));

    // A rejection after confirmation must not demote the verdict.
    store
        .flag_room_upstream_suspect(account_id, &room_id, "M_FORBIDDEN: yet again")
        .await
        .expect("flag suspect post-gone");
    assert!(store
        .rooms_gone_upstream(account_id)
        .await
        .expect("gone")
        .contains(&room_id));

    common::cleanup_account(&pool, account_id).await;
}

/// Clearing is how a room that comes back recovers, is safe to fire
/// unconditionally on a successful call, and is scoped to one account's row.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn clearing_is_idempotent_and_account_scoped() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "reconcile-clear").await;
    let other_account_id = test_account(&store, "reconcile-clear-other").await;
    let room_id = format!("!shared-{}:localhost", Uuid::new_v4());

    // Clearing a room that was never flagged is a no-op, not an error — callers
    // fire this on every success rather than reading first.
    store
        .clear_room_upstream_reconcile(account_id, &room_id)
        .await
        .expect("clear unflagged");

    for account in [account_id, other_account_id] {
        store
            .flag_room_upstream_suspect(account, &room_id, "M_FORBIDDEN")
            .await
            .expect("flag suspect");
        assert!(
            store
                .mark_room_upstream_gone(account, &room_id, "probe")
                .await
                .expect("mark gone"),
            "a suspect room is promoted"
        );
    }

    store
        .clear_room_upstream_reconcile(account_id, &room_id)
        .await
        .expect("clear");

    assert!(store
        .rooms_gone_upstream(account_id)
        .await
        .expect("gone")
        .is_empty());
    assert!(
        store
            .rooms_gone_upstream(other_account_id)
            .await
            .expect("gone for other account")
            .contains(&room_id),
        "one account's recovery leaves the other's verdict alone"
    );

    common::cleanup_account(&pool, account_id).await;
    common::cleanup_account(&pool, other_account_id).await;
}

/// A probe's verdict must not resurrect suspicion a concurrent success cleared.
///
/// The probe holds an upstream round trip open for seconds; a genuine
/// room-scoped call can succeed in that window, and its `clear` is newer and
/// stronger evidence than the verdict the probe is still carrying. Writing the
/// stale verdict back would pin the room's unread count to zero *and* hide it
/// from every future probe, which read only `suspect` rows — so nothing but
/// another lucky success would ever undo it.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_cleared_room_is_not_promoted_by_an_in_flight_probe() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "reconcile-race").await;
    let room_id = format!("!raced-{}:localhost", Uuid::new_v4());

    store
        .flag_room_upstream_suspect(account_id, &room_id, "M_FORBIDDEN")
        .await
        .expect("flag suspect");

    // ...the probe for it is now in flight, and a real call succeeds.
    store
        .clear_room_upstream_reconcile(account_id, &room_id)
        .await
        .expect("clear on success");

    // The probe returns with its stale rejection.
    assert!(
        !store
            .mark_room_upstream_gone(account_id, &room_id, "probe: M_FORBIDDEN")
            .await
            .expect("mark gone"),
        "the promotion does not apply to a row that is no longer suspect"
    );
    assert!(
        store
            .rooms_gone_upstream(account_id)
            .await
            .expect("gone")
            .is_empty(),
        "a room proved reachable stays reachable"
    );
    assert!(
        store
            .suspect_upstream_rooms(account_id)
            .await
            .expect("suspects")
            .is_empty(),
        "and it is not left suspect either"
    );

    common::cleanup_account(&pool, account_id).await;
}

/// Read `first_flagged_at` directly: the store API deliberately doesn't expose
/// it (nothing in axon reads it yet — it exists for debugging a wrong verdict),
/// but the insert-if-absent guarantee is only observable through it.
async fn first_flagged_at(
    pool: &sqlx_postgres::PgPool,
    account_id: Uuid,
    room_id: &str,
) -> chrono::DateTime<chrono::Utc> {
    use sqlx_core::row::Row;
    let row = sqlx_core::query::query(
        "SELECT first_flagged_at FROM room_upstream_reconcile \
         WHERE account_id = $1 AND room_id = $2",
    )
    .bind(account_id)
    .bind(room_id)
    .fetch_one(pool)
    .await
    .expect("read first_flagged_at");
    row.try_get("first_flagged_at").expect("first_flagged_at")
}
