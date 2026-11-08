//! The space discovery cache (ADR 0128 decisions 3-6): rows shared by
//! spaces, room-summary refresh state, and cleanup on leave, purge rebuild,
//! and account removal.
mod common;
use axon_store::{
    CrawlBegin, CrawlLimits, CrawlStep, DiscoveredEntry, RoomStateUpsert, Store, SummaryLimits,
    SummaryStep,
};
use serde_json::json;
use uuid::Uuid;

const SPACE_A: &str = "!space-a:localhost";
const SPACE_B: &str = "!space-b:localhost";
const CHILD: &str = "!child:localhost";

const LIMITS: CrawlLimits = CrawlLimits {
    page_limit: 50,
    max_pages: 40,
    max_cursor_restarts: 1,
    crawl_ttl_ms: 6 * 3600 * 1000,
    min_backoff_ms: 60_000,
    max_backoff_ms: 1_800_000,
};

async fn state(
    store: &Store,
    account_id: Uuid,
    user: &str,
    room_id: &str,
    event_type: &str,
    key: &str,
    content: serde_json::Value,
) {
    store
        .upsert_room_state_for_local_user(
            &RoomStateUpsert {
                account_id,
                room_id,
                event_type,
                state_key: key,
                event_id: &format!("$state-{}", Uuid::new_v4()),
                sender: user,
                origin_ts: chrono::Utc::now().timestamp_millis(),
                content: Some(content),
            },
            Some(user),
        )
        .await
        .unwrap();
}

/// An account joined to both spaces, each linking `CHILD`.
async fn setup(store: &Store) -> (Uuid, String) {
    let user = format!("@discovery-{}:localhost", Uuid::new_v4());
    let account_id = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .unwrap()
        .account_id;
    for space in [SPACE_A, SPACE_B] {
        common::insert_message(store, account_id, space, 1, "fixture").await;
        state(
            store,
            account_id,
            &user,
            space,
            "m.room.member",
            &user,
            json!({"membership": "join"}),
        )
        .await;
        state(
            store,
            account_id,
            &user,
            space,
            "m.space.child",
            CHILD,
            json!({"via": ["localhost"]}),
        )
        .await;
    }
    (account_id, user)
}

/// Crawl `space` in one complete page that returns `CHILD`.
async fn crawl(store: &Store, account_id: Uuid, space: &str, name: &str) {
    let epoch = store
        .joined_epoch(account_id, space)
        .await
        .unwrap()
        .unwrap();
    let CrawlBegin::Page(position) = store
        .begin_space_crawl(account_id, space, epoch, LIMITS)
        .await
        .unwrap()
    else {
        panic!("a new crawl starts");
    };
    let write = store
        .record_space_crawl(
            account_id,
            space,
            epoch,
            position.generation,
            LIMITS,
            &CrawlStep::Page {
                entries: vec![DiscoveredEntry {
                    room_id: CHILD.to_owned(),
                    summary: json!({ "name": name }),
                    withheld: Vec::new(),
                }],
                unidentified: 0,
                next_batch: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(write.outcome, "complete");
    assert_eq!(write.cached, 1);
}

async fn name(store: &Store, account_id: Uuid) -> Option<String> {
    store
        .room_discovery(account_id, CHILD)
        .await
        .unwrap()
        .and_then(|row| row.summary)
        .and_then(|summary| summary["name"].as_str().map(str::to_owned))
}

async fn crawls(store: &Store, account_id: Uuid) -> i64 {
    let (count,): (i64,) =
        sqlx_core::query_as::query_as("SELECT count(*) FROM space_discovery WHERE account_id = $1")
            .bind(account_id)
            .fetch_one(store.pool())
            .await
            .unwrap();
    count
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_row_shared_by_two_spaces_outlives_one_leave_and_not_both() {
    let store = common::migrated_store().await;
    let (account_id, user) = setup(&store).await;
    crawl(&store, account_id, SPACE_A, "from A").await;
    crawl(&store, account_id, SPACE_B, "from B").await;
    assert_eq!(
        name(&store, account_id).await.as_deref(),
        Some("from B"),
        "the latest answer wins"
    );

    let leave = json!({"membership": "leave"});
    state(
        &store,
        account_id,
        &user,
        SPACE_A,
        "m.room.member",
        &user,
        leave.clone(),
    )
    .await;
    assert_eq!(crawls(&store, account_id).await, 1, "A's crawl is gone");
    assert_eq!(
        name(&store, account_id).await.as_deref(),
        Some("from B"),
        "B still links the child"
    );

    state(
        &store,
        account_id,
        &user,
        SPACE_B,
        "m.room.member",
        &user,
        leave,
    )
    .await;
    assert_eq!(crawls(&store, account_id).await, 0);
    assert_eq!(name(&store, account_id).await, None);
    common::cleanup_account(store.pool(), account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_summary_rebuild_and_account_removal_delete_discovery() {
    let store = common::migrated_store().await;
    let (account_id, _) = setup(&store).await;
    crawl(&store, account_id, SPACE_A, "A").await;
    let epoch = store
        .joined_epoch(account_id, SPACE_A)
        .await
        .unwrap()
        .unwrap();

    store.rebuild_room_summaries(account_id).await.unwrap();
    assert_eq!(
        crawls(&store, account_id).await,
        0,
        "the rebuilt rows start over"
    );
    let rebuilt = store
        .joined_epoch(account_id, SPACE_A)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(rebuilt, epoch);
    assert_eq!(
        store
            .begin_space_crawl(account_id, SPACE_A, epoch, LIMITS)
            .await
            .unwrap(),
        CrawlBegin::Fenced,
        "a job from before the rebuild cannot start"
    );

    crawl(&store, account_id, SPACE_A, "A").await;
    assert!(name(&store, account_id).await.is_some());
    store.delete_account_row(account_id).await.unwrap();
    let (rows,): (i64,) = sqlx_core::query_as::query_as(
        "SELECT (SELECT count(*) FROM room_discovery WHERE account_id = $1) \
              + (SELECT count(*) FROM space_discovery WHERE account_id = $1)",
    )
    .bind(account_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(rows, 0, "account removal cascades");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_page_from_before_a_leave_and_rejoin_is_fenced_even_in_the_same_generation() {
    let store = common::migrated_store().await;
    let (account_id, user) = setup(&store).await;
    let before = store
        .joined_epoch(account_id, SPACE_A)
        .await
        .unwrap()
        .unwrap();
    let CrawlBegin::Page(stale) = store
        .begin_space_crawl(account_id, SPACE_A, before, LIMITS)
        .await
        .unwrap()
    else {
        panic!("a new crawl starts");
    };
    for membership in ["leave", "join"] {
        state(
            &store,
            account_id,
            &user,
            SPACE_A,
            "m.room.member",
            &user,
            json!({ "membership": membership }),
        )
        .await;
    }
    // A new crawl after the rejoin starts over at generation 1 too.
    let after = store
        .joined_epoch(account_id, SPACE_A)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(after, before);
    let CrawlBegin::Page(fresh) = store
        .begin_space_crawl(account_id, SPACE_A, after, LIMITS)
        .await
        .unwrap()
    else {
        panic!("the rejoined space crawls again");
    };
    assert_eq!(fresh.generation, stale.generation);
    let write = store
        .record_space_crawl(
            account_id,
            SPACE_A,
            before,
            stale.generation,
            LIMITS,
            &CrawlStep::Page {
                entries: vec![DiscoveredEntry {
                    room_id: CHILD.to_owned(),
                    summary: json!({ "name": "stale" }),
                    withheld: Vec::new(),
                }],
                unidentified: 0,
                next_batch: None,
            },
        )
        .await
        .unwrap();
    assert!(write.fenced, "the membership epoch fences it");
    assert_eq!(name(&store, account_id).await, None);
    common::cleanup_account(store.pool(), account_id).await;
}

const SUMMARY_LIMITS: SummaryLimits = SummaryLimits {
    summary_ttl_ms: 3600 * 1000,
    min_backoff_ms: 60_000,
    max_backoff_ms: 1_800_000,
};

async fn refresh_rows(store: &Store, account_id: Uuid) -> i64 {
    let (count,): (i64,) = sqlx_core::query_as::query_as(
        "SELECT count(*) FROM room_discovery_refresh WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    count
}

async fn fail_summary(store: &Store, account_id: Uuid) {
    let write = store
        .record_room_summary(
            account_id,
            CHILD,
            SUMMARY_LIMITS,
            &SummaryStep::Failure {
                category: "timeout",
                retry_after_ms: None,
            },
        )
        .await
        .unwrap();
    assert!(!write.fenced && write.changed);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn summary_refresh_rows_go_when_no_joined_space_links_the_room() {
    let store = common::migrated_store().await;
    let (account_id, user) = setup(&store).await;
    fail_summary(&store, account_id).await;
    assert_eq!(refresh_rows(&store, account_id).await, 1);
    let leave = json!({"membership": "leave"});
    state(
        &store,
        account_id,
        &user,
        SPACE_A,
        "m.room.member",
        &user,
        leave.clone(),
    )
    .await;
    assert_eq!(
        refresh_rows(&store, account_id).await,
        1,
        "B still links it"
    );
    state(
        &store,
        account_id,
        &user,
        SPACE_B,
        "m.room.member",
        &user,
        leave,
    )
    .await;
    assert_eq!(refresh_rows(&store, account_id).await, 0);
    common::cleanup_account(store.pool(), account_id).await;

    // A complete crawl's orphan step drops a refresh row whose links are gone.
    let (account_id, user) = setup(&store).await;
    fail_summary(&store, account_id).await;
    for space in [SPACE_A, SPACE_B] {
        state(
            &store,
            account_id,
            &user,
            space,
            "m.space.child",
            CHILD,
            json!({}),
        )
        .await;
    }
    let epoch = store
        .joined_epoch(account_id, SPACE_A)
        .await
        .unwrap()
        .unwrap();
    let CrawlBegin::Page(position) = store
        .begin_space_crawl(account_id, SPACE_A, epoch, LIMITS)
        .await
        .unwrap()
    else {
        panic!("a new crawl starts");
    };
    let write = store
        .record_space_crawl(
            account_id,
            SPACE_A,
            epoch,
            position.generation,
            LIMITS,
            &CrawlStep::Page {
                entries: Vec::new(),
                unidentified: 0,
                next_batch: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(write.outcome, "complete");
    assert_eq!(refresh_rows(&store, account_id).await, 0);

    // Account removal takes it too.
    common::cleanup_account(store.pool(), account_id).await;
    let (account_id, _) = setup(&store).await;
    fail_summary(&store, account_id).await;
    store.delete_account_row(account_id).await.unwrap();
    assert_eq!(
        refresh_rows(&store, account_id).await,
        0,
        "account removal cascades"
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn the_fallback_crawl_is_the_first_linking_space_whose_crawl_is_due() {
    let store = common::migrated_store().await;
    let (account_id, _) = setup(&store).await;
    assert_eq!(
        store
            .room_summary_fallback_space(account_id, CHILD)
            .await
            .unwrap()
            .as_deref(),
        Some(SPACE_A)
    );
    crawl(&store, account_id, SPACE_A, "A").await;
    assert_eq!(
        store
            .room_summary_fallback_space(account_id, CHILD)
            .await
            .unwrap()
            .as_deref(),
        Some(SPACE_B),
        "A's crawl is fresh"
    );
    crawl(&store, account_id, SPACE_B, "B").await;
    assert_eq!(
        store
            .room_summary_fallback_space(account_id, CHILD)
            .await
            .unwrap(),
        None
    );
    common::cleanup_account(store.pool(), account_id).await;
}
