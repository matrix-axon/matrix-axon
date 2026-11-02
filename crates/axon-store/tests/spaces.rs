//! Integration tests for the space-hierarchy reads (`space_children`,
//! `space_parents`; issue #404, ADR 0084).
//!
//! Like the other store integration tests these need a running Postgres and
//! are `#[ignore]`d by default:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store -- --ignored
//! ```

mod common;

use axon_store::{RoomStateUpsert, Store};
use common::{cleanup_account, migrated_store, raw_pool, test_account};
use serde_json::{json, Value};
use uuid::Uuid;

async fn set_space_child(
    store: &Store,
    account_id: Uuid,
    space_room_id: &str,
    child_room_id: &str,
    origin_ts: i64,
    content: Value,
) {
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id: space_room_id,
            event_type: "m.space.child",
            state_key: child_room_id,
            event_id: &format!("$child-{}:localhost", Uuid::new_v4()),
            sender: "@alice:localhost",
            origin_ts,
            content: Some(content),
        })
        .await
        .expect("m.space.child state");
}

async fn set_space_parent(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    parent_room_id: &str,
    content: Value,
) {
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id,
            event_type: "m.space.parent",
            state_key: parent_room_id,
            event_id: &format!("$parent-{}:localhost", Uuid::new_v4()),
            sender: "@alice:localhost",
            origin_ts: 1,
            content: Some(content),
        })
        .await
        .expect("m.space.parent state");
}

/// Give `room_id` a name/avatar (and optionally an `m.room.create` `type`) so
/// the space reads' enrichment has something to find.
async fn set_room_display(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    name: &str,
    avatar_url: &str,
    room_type: Option<&str>,
) {
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id,
            event_type: "m.room.name",
            state_key: "",
            event_id: &format!("$name-{}:localhost", Uuid::new_v4()),
            sender: "@alice:localhost",
            origin_ts: 1,
            content: Some(json!({ "name": name })),
        })
        .await
        .expect("name");
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id,
            event_type: "m.room.avatar",
            state_key: "",
            event_id: &format!("$avatar-{}:localhost", Uuid::new_v4()),
            sender: "@alice:localhost",
            origin_ts: 1,
            content: Some(json!({ "url": avatar_url })),
        })
        .await
        .expect("avatar");
    if let Some(room_type) = room_type {
        store
            .upsert_room_state(&RoomStateUpsert {
                account_id,
                room_id,
                event_type: "m.room.create",
                state_key: "",
                event_id: &format!("$create-{}:localhost", Uuid::new_v4()),
                sender: "@alice:localhost",
                origin_ts: 1,
                content: Some(json!({ "type": room_type })),
            })
            .await
            .expect("create");
    }
}

/// MSC1772 ordering (`order` string ascending, absent sorts last) and the
/// per-child enrichment: a known plain room, a completely unknown room, and a
/// known nested space, each exercising a different combination of "known
/// room" x "has `order`" x "is itself a space."
#[tokio::test]
#[ignore = "requires Postgres"]
async fn space_children_orders_by_msc1772_and_enriches_known_children() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "space-children").await;
    let space_room_id = format!("!space-{}:localhost", Uuid::new_v4());

    let child_a = format!("!child-a-{}:localhost", Uuid::new_v4());
    let child_b = format!("!child-b-{}:localhost", Uuid::new_v4());
    let child_space = format!("!child-space-{}:localhost", Uuid::new_v4());

    // order "b", never joined/known to Axon.
    set_space_child(
        &store,
        account_id,
        &space_room_id,
        &child_b,
        1_000,
        json!({ "via": [], "order": "b" }),
    )
    .await;
    // order "a" (sorts before "b"), known plain room.
    set_space_child(
        &store,
        account_id,
        &space_room_id,
        &child_a,
        2_000,
        json!({ "via": ["hs.example"], "order": "a", "suggested": true }),
    )
    .await;
    set_room_display(
        &store,
        account_id,
        &child_a,
        "Child A",
        "mxc://hs/child-a",
        None,
    )
    .await;
    // No `order` at all -> sorts after every child that has one, known nested space.
    set_space_child(
        &store,
        account_id,
        &space_room_id,
        &child_space,
        500,
        json!({ "via": ["hs.example"] }),
    )
    .await;
    set_room_display(
        &store,
        account_id,
        &child_space,
        "Nested Space",
        "mxc://hs/child-space",
        Some("m.space"),
    )
    .await;

    let children = store
        .space_children(account_id, &space_room_id)
        .await
        .expect("space_children");
    assert_eq!(children.len(), 3);

    assert_eq!(children[0].room_id, child_a);
    assert_eq!(children[0].order.as_deref(), Some("a"));
    assert!(children[0].suggested);
    assert_eq!(children[0].via, vec!["hs.example".to_owned()]);
    assert_eq!(children[0].name.as_deref(), Some("Child A"));
    assert_eq!(children[0].avatar_url.as_deref(), Some("mxc://hs/child-a"));
    assert_eq!(children[0].room_type, None);

    assert_eq!(children[1].room_id, child_b);
    assert_eq!(children[1].order.as_deref(), Some("b"));
    assert!(!children[1].suggested);
    assert!(children[1].via.is_empty());
    assert_eq!(children[1].name, None);
    assert_eq!(children[1].avatar_url, None);
    assert_eq!(children[1].room_type, None);

    assert_eq!(children[2].room_id, child_space);
    assert_eq!(children[2].order, None);
    assert_eq!(children[2].name.as_deref(), Some("Nested Space"));
    assert_eq!(
        children[2].avatar_url.as_deref(),
        Some("mxc://hs/child-space")
    );
    assert_eq!(children[2].room_type.as_deref(), Some("m.space"));

    cleanup_account(&pool, account_id).await;
}

/// Children with no `order` fall back to `origin_ts` then `room_id` — the
/// second and third MSC1772 tiebreaks, not exercised by the enrichment test
/// above (which only has one order-less child).
#[tokio::test]
#[ignore = "requires Postgres"]
async fn space_children_without_order_tie_break_on_origin_ts_then_room_id() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "space-children-tie").await;
    let space_room_id = format!("!space-tie-{}:localhost", Uuid::new_v4());

    let late = format!("!tie-late-{}:localhost", Uuid::new_v4());
    let early = format!("!tie-early-{}:localhost", Uuid::new_v4());
    // Fixed (non-random) suffixes so the lexical room_id comparison at equal
    // origin_ts is deterministic.
    let same_ts_a = "!tie-same-a:localhost".to_owned();
    let same_ts_b = "!tie-same-b:localhost".to_owned();

    set_space_child(&store, account_id, &space_room_id, &late, 2_000, json!({})).await;
    set_space_child(&store, account_id, &space_room_id, &early, 1_000, json!({})).await;
    set_space_child(
        &store,
        account_id,
        &space_room_id,
        &same_ts_b,
        1_500,
        json!({}),
    )
    .await;
    set_space_child(
        &store,
        account_id,
        &space_room_id,
        &same_ts_a,
        1_500,
        json!({}),
    )
    .await;

    let children = store
        .space_children(account_id, &space_room_id)
        .await
        .expect("space_children");
    let room_ids: Vec<&str> = children.iter().map(|c| c.room_id.as_str()).collect();
    assert_eq!(room_ids, vec![&early, &same_ts_a, &same_ts_b, &late]);

    cleanup_account(&pool, account_id).await;
}

/// The reverse lookup: a room's parent spaces, with the same enrichment as
/// children but `canonical` instead of `order`/`suggested`.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn space_parents_reads_canonical_flag_and_enriches_known_parents() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "space-parents").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());

    let parent_known = format!("!parent-known-{}:localhost", Uuid::new_v4());
    let parent_unknown = format!("!parent-unknown-{}:localhost", Uuid::new_v4());

    set_space_parent(
        &store,
        account_id,
        &room_id,
        &parent_known,
        json!({ "via": ["hs.example"], "canonical": true }),
    )
    .await;
    set_room_display(
        &store,
        account_id,
        &parent_known,
        "Parent Space",
        "mxc://hs/parent",
        Some("m.space"),
    )
    .await;
    set_space_parent(
        &store,
        account_id,
        &room_id,
        &parent_unknown,
        json!({ "via": [] }),
    )
    .await;

    let parents = store
        .space_parents(account_id, &room_id)
        .await
        .expect("space_parents");
    assert_eq!(parents.len(), 2);

    let known = parents
        .iter()
        .find(|p| p.room_id == parent_known)
        .expect("known parent present");
    assert!(known.canonical);
    assert_eq!(known.via, vec!["hs.example".to_owned()]);
    assert_eq!(known.name.as_deref(), Some("Parent Space"));
    assert_eq!(known.avatar_url.as_deref(), Some("mxc://hs/parent"));
    assert_eq!(known.room_type.as_deref(), Some("m.space"));

    let unknown = parents
        .iter()
        .find(|p| p.room_id == parent_unknown)
        .expect("unknown parent present");
    assert!(!unknown.canonical);
    assert_eq!(unknown.name, None);
    assert_eq!(unknown.avatar_url, None);
    assert_eq!(unknown.room_type, None);

    cleanup_account(&pool, account_id).await;
}

/// An unknown room, or one with no space state, yields an empty list — not an
/// error — for both directions.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn space_reads_are_empty_for_a_room_with_no_space_state() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "space-empty").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());

    assert!(store
        .space_children(account_id, &room_id)
        .await
        .expect("space_children")
        .is_empty());
    assert!(store
        .space_parents(account_id, &room_id)
        .await
        .expect("space_parents")
        .is_empty());

    cleanup_account(&pool, account_id).await;
}

/// A room can hold an arbitrary number of `m.space.child`/`m.space.parent`
/// state keys (bounded only by upstream federation, not by Axon), so both
/// reads must cap rather than return everything unbounded. `SPACE_HIERARCHY_CAP`
/// itself is a private store constant, so this pins the current cap value
/// (1000) rather than importing it — if the store's cap ever changes, update
/// `over_cap`/the assertions here to match.
const SPACE_HIERARCHY_CAP: i64 = 1000;

/// Bulk-insert `count` distinct `m.space.child` rows for `space_room_id`, each
/// with a unique `origin_ts` (`1..=count`) and no `order` — one round trip via
/// `generate_series` rather than `count` sequential awaited upserts, so a
/// >1000-row test stays fast.
async fn bulk_insert_space_children(
    pool: &sqlx_postgres::PgPool,
    account_id: Uuid,
    space_room_id: &str,
    count: i64,
) {
    sqlx_core::query::query(
        "INSERT INTO room_state \
             (account_id, room_id, event_type, state_key, event_id, sender, origin_ts, content) \
         SELECT $1, $2, 'm.space.child', '!child-' || g || ':localhost', \
                '$child-' || g || ':localhost', '@alice:localhost', g, '{}'::jsonb \
         FROM generate_series(1, $3) AS g",
    )
    .bind(account_id)
    .bind(space_room_id)
    .bind(count)
    .execute(pool)
    .await
    .expect("bulk insert space children");
}

/// Same as [`bulk_insert_space_children`] but for `m.space.parent`, state-keyed
/// on the parent room id rather than a child's.
async fn bulk_insert_space_parents(
    pool: &sqlx_postgres::PgPool,
    account_id: Uuid,
    room_id: &str,
    count: i64,
) {
    sqlx_core::query::query(
        "INSERT INTO room_state \
             (account_id, room_id, event_type, state_key, event_id, sender, origin_ts, content) \
         SELECT $1, $2, 'm.space.parent', '!parent-' || g || ':localhost', \
                '$parent-' || g || ':localhost', '@alice:localhost', g, '{}'::jsonb \
         FROM generate_series(1, $3) AS g",
    )
    .bind(account_id)
    .bind(room_id)
    .bind(count)
    .execute(pool)
    .await
    .expect("bulk insert space parents");
}

/// `space_children` truncates to `SPACE_HIERARCHY_CAP` rows, keeping the
/// leading (lowest `origin_ts`, since none of these have an `order`) entries
/// per the MSC1772 sort — not an arbitrary subset.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn space_children_are_capped_at_space_hierarchy_cap() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "space-children-cap").await;
    let space_room_id = format!("!space-cap-{}:localhost", Uuid::new_v4());

    let over_cap = SPACE_HIERARCHY_CAP + 5;
    bulk_insert_space_children(&pool, account_id, &space_room_id, over_cap).await;

    let children = store
        .space_children(account_id, &space_room_id)
        .await
        .expect("space_children");
    assert_eq!(children.len() as i64, SPACE_HIERARCHY_CAP);
    // origin_ts 1..=SPACE_HIERARCHY_CAP survive; the last 5 (over the cap) don't.
    assert_eq!(children[0].room_id, "!child-1:localhost");
    assert_eq!(
        children[(SPACE_HIERARCHY_CAP - 1) as usize].room_id,
        format!("!child-{SPACE_HIERARCHY_CAP}:localhost")
    );
    assert!(!children
        .iter()
        .any(|c| c.room_id == format!("!child-{over_cap}:localhost")));

    cleanup_account(&pool, account_id).await;
}

/// `space_parents` is capped the same way as `space_children`.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn space_parents_are_capped_at_space_hierarchy_cap() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "space-parents-cap").await;
    let room_id = format!("!room-cap-{}:localhost", Uuid::new_v4());

    bulk_insert_space_parents(&pool, account_id, &room_id, SPACE_HIERARCHY_CAP + 5).await;

    let parents = store
        .space_parents(account_id, &room_id)
        .await
        .expect("space_parents");
    assert_eq!(parents.len() as i64, SPACE_HIERARCHY_CAP);

    cleanup_account(&pool, account_id).await;
}

// ---------------------------------------------------------------------------
// Paged hierarchy read (ADR 0128 decision 1).
// ---------------------------------------------------------------------------

/// A valid link: the spec ignores `m.space.child` content without a `via`.
fn link(order: Option<&str>) -> Value {
    match order {
        Some(order) => json!({ "via": ["localhost"], "order": order }),
        None => json!({ "via": ["localhost"] }),
    }
}

/// Walk every page of `space_room_id` at `limit`, returning the room ids in
/// read order and checking the per-page invariants on the way.
async fn walk_hierarchy(
    store: &Store,
    account_id: Uuid,
    space_room_id: &str,
    limit: i64,
) -> Vec<String> {
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let page = store
            .space_hierarchy_page(account_id, space_room_id, after.as_ref(), limit)
            .await
            .expect("space_hierarchy_page");
        assert!(page.rows.len() as i64 <= limit);
        if page.has_more {
            assert_eq!(page.rows.len() as i64, limit, "only the last page is short");
        }
        seen.extend(page.rows.iter().map(|r| r.child.room_id.clone()));
        if !page.has_more {
            return seen;
        }
        after = Some(
            page.rows
                .last()
                .expect("a page with more is non-empty")
                .cursor(),
        );
    }
}

/// Bulk-insert `count` valid links, `!h-<g>:localhost`, whose `order` is
/// `o-<g mod 7>` for two of every three rows and absent for the third, and
/// whose `origin_ts` collides in groups of five, so all three sort keys
/// decide some boundary.
async fn bulk_insert_valid_links(
    pool: &sqlx_postgres::PgPool,
    account_id: Uuid,
    space_room_id: &str,
    count: i64,
) {
    sqlx_core::query::query(
        "INSERT INTO room_state \
             (account_id, room_id, event_type, state_key, event_id, sender, origin_ts, content) \
         SELECT $1, $2, 'm.space.child', '!h-' || g || ':localhost', \
                '$h-' || g || ':localhost', '@alice:localhost', g / 5, \
                CASE WHEN g % 3 = 0 THEN '{\"via\":[\"localhost\"]}'::jsonb \
                     ELSE jsonb_build_object('via', jsonb_build_array('localhost'), \
                                             'order', 'o-' || (g % 7)) END \
         FROM generate_series(1, $3) AS g",
    )
    .bind(account_id)
    .bind(space_room_id)
    .bind(count)
    .execute(pool)
    .await
    .expect("bulk insert valid links");
}

/// Paging a large space at several page sizes yields every link exactly once,
/// in the same order as one page holding all of them, with the absent-`order`
/// group last and an honest total — including a size that divides the space
/// exactly, where the last full page must still report no more.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn space_hierarchy_pages_every_link_once_in_msc1772_order() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "space-hier-paging").await;
    let space_room_id = format!("!space-hier-{}:localhost", Uuid::new_v4());
    let count = 1_200;
    bulk_insert_valid_links(&pool, account_id, &space_room_id, count).await;

    let whole = store
        .space_hierarchy_page(account_id, &space_room_id, None, 500)
        .await
        .expect("first page");
    assert_eq!(whole.total_children, count);
    assert!(whole.has_more);

    let reference = walk_hierarchy(&store, account_id, &space_room_id, 500).await;
    assert_eq!(reference.len() as i64, count);
    let mut unique = reference.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len() as i64, count, "no link appears twice");

    // Ordered groups first, then the absent group (every third id).
    let first_absent = reference
        .iter()
        .position(|id| {
            id.trim_start_matches("!h-")
                .trim_end_matches(":localhost")
                .parse::<i64>()
                .unwrap()
                % 3
                == 0
        })
        .expect("an absent-order link");
    assert_eq!(first_absent as i64, count - count / 3);

    // 1,200 = 8 × 150 exactly; 7 and 1 do not divide into the same pages.
    for limit in [150, 7, 1] {
        assert_eq!(
            walk_hierarchy(&store, account_id, &space_room_id, limit).await,
            reference,
            "limit {limit}"
        );
    }

    cleanup_account(&pool, account_id).await;
}

/// The key decides each boundary on its own: `order` by code point (so "B"
/// sorts before "a", whatever the database collation), then `origin_ts`, then
/// room id by code point. A non-string, overlong, or non-printable `order` is
/// absent, sorts last, and reads back as `None`.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn space_hierarchy_sort_key_and_order_normalization() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "space-hier-order").await;
    let space = format!("!space-hier-order-{}:localhost", Uuid::new_v4());

    let cases: [(&str, i64, Value); 9] = [
        ("!lower:localhost", 1, link(Some("a"))),
        ("!upper:localhost", 9, link(Some("B"))),
        ("!same-order-late:localhost", 5, link(Some("m"))),
        ("!same-order-early:localhost", 2, link(Some("m"))),
        ("!tie-Z:localhost", 3, link(Some("t"))),
        ("!tie-a:localhost", 3, link(Some("t"))),
        (
            "!numeric-order:localhost",
            0,
            json!({ "via": ["localhost"], "order": 5 }),
        ),
        ("!long-order:localhost", 1, link(Some(&"x".repeat(51)))),
        ("!unicode-order:localhost", 2, link(Some("é"))),
    ];
    for (child, ts, content) in &cases {
        set_space_child(&store, account_id, &space, child, *ts, content.clone()).await;
    }

    let page = store
        .space_hierarchy_page(account_id, &space, None, 100)
        .await
        .expect("page");
    let ids: Vec<&str> = page.rows.iter().map(|r| r.child.room_id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "!upper:localhost",
            "!lower:localhost",
            "!same-order-early:localhost",
            "!same-order-late:localhost",
            "!tie-Z:localhost",
            "!tie-a:localhost",
            // Absent group, by origin_ts.
            "!numeric-order:localhost",
            "!long-order:localhost",
            "!unicode-order:localhost",
        ]
    );
    let orders: Vec<Option<&str>> = page.rows.iter().map(|r| r.child.order.as_deref()).collect();
    assert_eq!(
        orders,
        [
            Some("B"),
            Some("a"),
            Some("m"),
            Some("m"),
            Some("t"),
            Some("t"),
            None,
            None,
            None
        ]
    );
    // A 50-character order is still valid.
    set_space_child(
        &store,
        account_id,
        &space,
        "!fifty:localhost",
        0,
        link(Some(&"0".repeat(50))),
    )
    .await;
    let page = store
        .space_hierarchy_page(account_id, &space, None, 1)
        .await
        .expect("page");
    assert_eq!(page.rows[0].child.room_id, "!fifty:localhost");

    cleanup_account(&pool, account_id).await;
}

/// Only current links count: no `via`, an empty or malformed `via`, a
/// redacted link, and an id over the spec's 255-byte room-id limit are left
/// out of both the rows and `total_children`. The legacy read still returns
/// the removed links, unchanged.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn space_hierarchy_counts_only_current_links() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "space-hier-links").await;
    let space = format!("!space-hier-links-{}:localhost", Uuid::new_v4());

    set_space_child(
        &store,
        account_id,
        &space,
        "!valid:localhost",
        1,
        link(None),
    )
    .await;
    set_space_child(
        &store,
        account_id,
        &space,
        "!removed:localhost",
        2,
        json!({}),
    )
    .await;
    set_space_child(
        &store,
        account_id,
        &space,
        "!empty-via:localhost",
        3,
        json!({ "via": [] }),
    )
    .await;
    set_space_child(
        &store,
        account_id,
        &space,
        "!string-via:localhost",
        4,
        json!({ "via": "localhost" }),
    )
    .await;
    set_space_child(
        &store,
        account_id,
        &space,
        "!number-via:localhost",
        5,
        json!({ "via": [1] }),
    )
    .await;
    set_space_child(
        &store,
        account_id,
        &space,
        "!blank-via:localhost",
        6,
        json!({ "via": [""] }),
    )
    .await;
    let long_id = format!("!{}:localhost", "x".repeat(250));
    set_space_child(&store, account_id, &space, &long_id, 7, link(None)).await;
    set_space_child(
        &store,
        account_id,
        &space,
        "!redacted:localhost",
        8,
        link(None),
    )
    .await;
    sqlx_core::query::query(
        "UPDATE room_state SET redacted = true, redaction_event_id = '$r:localhost' \
         WHERE account_id = $1 AND room_id = $2 AND state_key = '!redacted:localhost'",
    )
    .bind(account_id)
    .bind(&space)
    .execute(&pool)
    .await
    .expect("mark redacted");

    let page = store
        .space_hierarchy_page(account_id, &space, None, 100)
        .await
        .expect("page");
    let ids: Vec<&str> = page.rows.iter().map(|r| r.child.room_id.as_str()).collect();
    assert_eq!(ids, ["!valid:localhost"]);
    assert_eq!(page.total_children, 1);
    assert!(!page.has_more);

    let legacy = store
        .space_children(account_id, &space)
        .await
        .expect("legacy");
    assert_eq!(legacy.len(), 8, "the legacy read is unchanged");

    cleanup_account(&pool, account_id).await;
}

/// Links added and removed between pages never duplicate or skip a link that
/// was present for the whole walk, and the total tracks the change.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn space_hierarchy_is_stable_while_links_change_between_pages() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let account_id = test_account(&store, "space-hier-churn").await;
    let space = format!("!space-hier-churn-{}:localhost", Uuid::new_v4());
    for i in 0..10 {
        let order = format!("{i:02}");
        set_space_child(
            &store,
            account_id,
            &space,
            &format!("!c{i:02}:localhost"),
            1,
            link(Some(&order)),
        )
        .await;
    }

    let first = store
        .space_hierarchy_page(account_id, &space, None, 4)
        .await
        .expect("first page");
    let mut seen: Vec<String> = first.rows.iter().map(|r| r.child.room_id.clone()).collect();
    assert_eq!(
        seen,
        [
            "!c00:localhost",
            "!c01:localhost",
            "!c02:localhost",
            "!c03:localhost"
        ]
    );

    // Between pages: remove an already-read link and an unread one, add one
    // before the cursor and one after it.
    set_space_child(&store, account_id, &space, "!c01:localhost", 2, json!({})).await;
    set_space_child(&store, account_id, &space, "!c06:localhost", 2, json!({})).await;
    set_space_child(
        &store,
        account_id,
        &space,
        "!new-early:localhost",
        2,
        link(Some("00a")),
    )
    .await;
    set_space_child(
        &store,
        account_id,
        &space,
        "!new-late:localhost",
        2,
        link(Some("07a")),
    )
    .await;

    let mut after = first.rows.last().map(|r| r.cursor());
    loop {
        let page = store
            .space_hierarchy_page(account_id, &space, after.as_ref(), 4)
            .await
            .expect("page");
        assert_eq!(page.total_children, 10);
        seen.extend(page.rows.iter().map(|r| r.child.room_id.clone()));
        if !page.has_more {
            break;
        }
        after = page.rows.last().map(|r| r.cursor());
    }
    assert_eq!(
        seen,
        [
            "!c00:localhost",
            "!c01:localhost",
            "!c02:localhost",
            "!c03:localhost",
            "!c04:localhost",
            "!c05:localhost",
            "!c07:localhost",
            "!new-late:localhost",
            "!c08:localhost",
            "!c09:localhost",
        ]
    );

    // A cursor past the last row is an empty page that still reports the
    // total.
    let past = store
        .space_hierarchy_page(account_id, &space, after.as_ref(), 4)
        .await
        .expect("past the end");
    let end = past.rows.last().map(|r| r.cursor()).or(after);
    let empty = store
        .space_hierarchy_page(account_id, &space, end.as_ref(), 4)
        .await
        .expect("empty page");
    assert!(empty.rows.is_empty());
    assert!(!empty.has_more);
    assert_eq!(empty.total_children, 10);

    cleanup_account(&pool, account_id).await;
}

/// `joined` reflects the account's own positive membership in the child, and
/// the synced display fields are filled for children Axon knows.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn space_hierarchy_reports_membership_and_synced_fields() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let user_id = format!("@space-hier-member-{}:localhost", Uuid::new_v4());
    let account_id = store
        .upsert_account(&user_id, "https://hs.example.org")
        .await
        .expect("account")
        .account_id;
    let space = format!("!space-hier-member-{}:localhost", Uuid::new_v4());
    for (i, child) in [
        "!joined:localhost",
        "!left:localhost",
        "!other-joined:localhost",
        "!unknown:localhost",
    ]
    .iter()
    .enumerate()
    {
        set_space_child(&store, account_id, &space, child, i as i64, link(None)).await;
    }
    for (room, member, membership) in [
        ("!joined:localhost", user_id.as_str(), "join"),
        ("!left:localhost", user_id.as_str(), "leave"),
        ("!other-joined:localhost", "@someone-else:localhost", "join"),
    ] {
        store
            .upsert_room_state(&RoomStateUpsert {
                account_id,
                room_id: room,
                event_type: "m.room.member",
                state_key: member,
                event_id: &format!("$member-{}:localhost", Uuid::new_v4()),
                sender: member,
                origin_ts: 1,
                content: Some(json!({ "membership": membership })),
            })
            .await
            .expect("membership");
    }
    set_room_display(
        &store,
        account_id,
        "!joined:localhost",
        "Joined",
        "mxc://localhost/j",
        Some("m.space"),
    )
    .await;

    let page = store
        .space_hierarchy_page(account_id, &space, None, 100)
        .await
        .expect("page");
    let joined: Vec<(&str, bool)> = page
        .rows
        .iter()
        .map(|r| (r.child.room_id.as_str(), r.joined))
        .collect();
    assert_eq!(
        joined,
        [
            ("!joined:localhost", true),
            ("!left:localhost", false),
            ("!other-joined:localhost", false),
            ("!unknown:localhost", false),
        ]
    );
    let first = &page.rows[0].child;
    assert_eq!(first.name.as_deref(), Some("Joined"));
    assert_eq!(first.avatar_url.as_deref(), Some("mxc://localhost/j"));
    assert_eq!(first.room_type.as_deref(), Some("m.space"));
    assert_eq!(first.via, ["localhost"]);
    assert!(page.rows[3].child.name.is_none());

    // An unknown space, or another account's, is an empty page.
    let other = test_account(&store, "space-hier-other").await;
    for (account, room) in [(account_id, "!nope:localhost"), (other, space.as_str())] {
        let page = store
            .space_hierarchy_page(account, room, None, 100)
            .await
            .expect("empty");
        assert!(page.rows.is_empty());
        assert_eq!(page.total_children, 0);
    }

    cleanup_account(&pool, account_id).await;
    cleanup_account(&pool, other).await;
}
