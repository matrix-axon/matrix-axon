//! Integration tests for the room-state and account-data projections.
//!
//! Like the event-store tests these need a running Postgres and are `#[ignore]`d
//! by default. Run them with:
//!
//! ```sh
//! docker compose up -d postgres
//! # 5432 is the default; use your compose host port (e.g. 5433 if 5432 is taken).
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store -- --ignored
//! ```

mod common;

use axon_store::{AccountDataUpsert, RoomStateUpsert};
use common::test_account;
use serde_json::json;
use sqlx_core::row::Row;
use uuid::Uuid;

/// Room state upserts in place: re-setting a tuple overwrites the prior value,
/// and distinct (event_type, state_key) tuples coexist.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn room_state_upserts_current_value() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "rs").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());

    // m.room.name (singleton: state_key "").
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id: &room_id,
            event_type: "m.room.name",
            state_key: "",
            event_id: "$name1:localhost",
            sender: "@alice:localhost",
            origin_ts: 1_000,
            content: Some(json!({ "name": "First" })),
        })
        .await
        .expect("name v1");

    // A member tuple (state_key = user id) — distinct tuple, coexists.
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id: &room_id,
            event_type: "m.room.member",
            state_key: "@bob:localhost",
            event_id: "$mem1:localhost",
            sender: "@bob:localhost",
            origin_ts: 1_100,
            content: Some(json!({ "membership": "join" })),
        })
        .await
        .expect("member");

    // Newer name overwrites the singleton.
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id: &room_id,
            event_type: "m.room.name",
            state_key: "",
            event_id: "$name2:localhost",
            sender: "@alice:localhost",
            origin_ts: 2_000,
            content: Some(json!({ "name": "Renamed" })),
        })
        .await
        .expect("name v2");

    let name = store
        .room_state(account_id, &room_id, "m.room.name", "")
        .await
        .expect("read name")
        .expect("name present");
    assert_eq!(name.content.unwrap()["name"], "Renamed");
    assert_eq!(name.event_id, "$name2:localhost");

    // Both tuples are present; the member row is untouched by the name update.
    let member = store
        .room_state(account_id, &room_id, "m.room.member", "@bob:localhost")
        .await
        .expect("read member")
        .expect("member present");
    assert_eq!(member.content.unwrap()["membership"], "join");

    common::cleanup_account(&pool, account_id).await;
}

/// The freshness guard: an event with an older origin_ts must not clobber newer
/// state, regardless of arrival order.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn room_state_freshness_guard_rejects_older() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "fresh").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());

    // Newer first, then an older replay arrives.
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id: &room_id,
            event_type: "m.room.topic",
            state_key: "",
            event_id: "$new:localhost",
            sender: "@alice:localhost",
            origin_ts: 5_000,
            content: Some(json!({ "topic": "current" })),
        })
        .await
        .expect("set newer topic");
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id: &room_id,
            event_type: "m.room.topic",
            state_key: "",
            event_id: "$old:localhost",
            sender: "@alice:localhost",
            origin_ts: 1_000,
            content: Some(json!({ "topic": "stale" })),
        })
        .await
        .expect("set older topic");

    let topic = store
        .room_state(account_id, &room_id, "m.room.topic", "")
        .await
        .expect("read topic")
        .expect("topic present");
    assert_eq!(topic.content.unwrap()["topic"], "current");
    assert_eq!(
        topic.event_id, "$new:localhost",
        "older replay clobbered newer state"
    );

    common::cleanup_account(&pool, account_id).await;
}

/// `room_state_of_type` returns every state_key of one type, ordered — e.g. a
/// membership list.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn room_state_of_type_lists_members() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "members").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());

    for user in ["@carol:localhost", "@alice:localhost", "@bob:localhost"] {
        store
            .upsert_room_state(&RoomStateUpsert {
                account_id,
                room_id: &room_id,
                event_type: "m.room.member",
                state_key: user,
                event_id: &format!("$m-{user}"),
                sender: user,
                origin_ts: 1_000,
                content: Some(json!({ "membership": "join" })),
            })
            .await
            .expect("member");
    }

    let members = store
        .room_state_of_type(account_id, &room_id, "m.room.member")
        .await
        .expect("members");
    let keys: Vec<&str> = members.iter().map(|m| m.state_key.as_str()).collect();
    // Ordered by state_key ascending.
    assert_eq!(
        keys,
        ["@alice:localhost", "@bob:localhost", "@carol:localhost"]
    );

    common::cleanup_account(&pool, account_id).await;
}

/// Account data: global and per-room scopes are distinct, both upsert in place,
/// and a global row reads back with `room_id == None`.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn account_data_global_and_room_scopes() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "ad").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());

    // Global m.direct.
    store
        .upsert_account_data(&AccountDataUpsert {
            account_id,
            room_id: None,
            event_type: "m.direct",
            content: json!({ "@bob:localhost": [room_id] }),
        })
        .await
        .expect("global direct");

    // Per-room m.fully_read — same account, different scope, must not collide
    // with global even if a type name overlapped.
    store
        .upsert_account_data(&AccountDataUpsert {
            account_id,
            room_id: Some(&room_id),
            event_type: "m.fully_read",
            content: json!({ "event_id": "$read1:localhost" }),
        })
        .await
        .expect("room fully_read v1");

    // Update the per-room marker in place.
    store
        .upsert_account_data(&AccountDataUpsert {
            account_id,
            room_id: Some(&room_id),
            event_type: "m.fully_read",
            content: json!({ "event_id": "$read2:localhost" }),
        })
        .await
        .expect("room fully_read v2");

    let global = store
        .account_data(account_id, None, "m.direct")
        .await
        .expect("read global")
        .expect("global present");
    assert_eq!(global.room_id, None, "global scope reads back as None");
    assert_eq!(global.content["@bob:localhost"][0], room_id.as_str());

    let room = store
        .account_data(account_id, Some(&room_id), "m.fully_read")
        .await
        .expect("read room")
        .expect("room present");
    assert_eq!(room.room_id.as_deref(), Some(room_id.as_str()));
    assert_eq!(room.content["event_id"], "$read2:localhost");

    // A global lookup for the per-room type finds nothing — scopes don't bleed.
    assert!(store
        .account_data(account_id, None, "m.fully_read")
        .await
        .expect("read global fully_read")
        .is_none());

    common::cleanup_account(&pool, account_id).await;
}

/// Both projections cascade-delete with their account.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn state_and_account_data_cascade_with_account() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "cascade").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());

    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id: &room_id,
            event_type: "m.room.name",
            state_key: "",
            event_id: "$n:localhost",
            sender: "@alice:localhost",
            origin_ts: 1,
            content: Some(json!({ "name": "Doomed" })),
        })
        .await
        .expect("state");
    store
        .upsert_account_data(&AccountDataUpsert {
            account_id,
            room_id: None,
            event_type: "m.direct",
            content: json!({}),
        })
        .await
        .expect("account data");

    common::cleanup_account(&pool, account_id).await;

    // After the account is gone, both projections are empty for it.
    for table in ["room_state", "account_data"] {
        let n: i64 = sqlx_core::query::query(&format!(
            "SELECT count(*) AS c FROM {table} WHERE account_id = $1"
        ))
        .bind(account_id)
        .fetch_one(&pool)
        .await
        .expect("count")
        .try_get("c")
        .expect("c");
        assert_eq!(n, 0, "{table} should cascade-delete with the account");
    }
}

/// `apply_room_tag` creates the `m.tag` row, merges a second tag without
/// clobbering the first, and `remove_room_tag` drops only the named key
/// (ADR 0103 write-path upsert).
#[tokio::test]
#[ignore = "requires Postgres"]
async fn apply_and_remove_room_tag_merges_json() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = test_account(&store, "tag-upsert").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());

    let content = store
        .apply_room_tag(account_id, &room_id, "m.favourite", Some(0.5))
        .await
        .expect("set favourite");
    assert_eq!(content["tags"]["m.favourite"]["order"], 0.5);

    let content = store
        .apply_room_tag(account_id, &room_id, "u.work", None)
        .await
        .expect("set custom");
    assert_eq!(content["tags"]["m.favourite"]["order"], 0.5);
    assert_eq!(content["tags"]["u.work"], json!({}));

    let content = store
        .remove_room_tag(account_id, &room_id, "m.favourite")
        .await
        .expect("remove favourite")
        .expect("row existed");
    assert!(content["tags"].get("m.favourite").is_none());
    assert_eq!(content["tags"]["u.work"], json!({}));

    let row = store
        .account_data(account_id, Some(&room_id), "m.tag")
        .await
        .expect("read")
        .expect("present");
    assert_eq!(row.content, content);

    let missing = format!("!never-{}:localhost", Uuid::new_v4());
    assert!(
        store
            .remove_room_tag(account_id, &missing, "m.favourite")
            .await
            .expect("no-op remove")
            .is_none(),
        "unpinning a room with no m.tag row must not insert one"
    );
    assert!(store
        .account_data(account_id, Some(&missing), "m.tag")
        .await
        .expect("read missing")
        .is_none());

    common::cleanup_account(&pool, account_id).await;
}
