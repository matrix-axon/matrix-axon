//! DB-gated tests for `Store::room_previews` (#278): one read for many room
//! rows' latest-message previews, with the timeline read's rules.
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store --test room_previews -- --ignored
//! ```

mod common;

use axon_store::{
    NewEvent, RoomPreview, RoomStateUpsert, Store, ROOM_PREVIEW_BODY_CHARS, ROOM_PREVIEW_SCAN_LIMIT,
};
use common::insert_message;
use serde_json::{json, Value};
use uuid::Uuid;

/// `insert_message`'s sender.
const SENDER: &str = "@alice:localhost";
const BASE: i64 = 1_700_000_000_000;

fn new_room() -> String {
    format!("!preview-{}:localhost", Uuid::new_v4())
}

/// Insert any event. `body` goes to `decrypted_body_text`, as ingestion lifts
/// `content.body`; `state_key` lands in `raw_event`, where the timeline reads it.
#[allow(clippy::too_many_arguments)]
async fn insert_event(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    origin_ts: i64,
    event_type: &str,
    content: Value,
    state_key: Option<&str>,
    relates_to: Option<Value>,
    redacts: Option<&str>,
) -> String {
    let event_id = format!("$evt-{}:localhost", Uuid::new_v4());
    let body = content
        .get("body")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut raw = json!({ "type": event_type, "content": content });
    if let Some(key) = state_key {
        raw["state_key"] = json!(key);
    }
    if let Some(target) = redacts {
        raw["redacts"] = json!(target);
    }
    store
        .upsert_event(&NewEvent {
            event_id: &event_id,
            room_id,
            account_id,
            sender: SENDER,
            origin_ts,
            event_type,
            content: Some(content),
            raw_event: raw,
            megolm_session_id: None,
            redacts,
            relates_to,
            decrypted_body_text: body.as_deref(),
        })
        .await
        .expect("insert event");
    event_id
}

async fn set_display_name(store: &Store, account_id: Uuid, room_id: &str, name: Value) {
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id,
            event_type: "m.room.member",
            state_key: SENDER,
            event_id: &format!("$member-{}:localhost", Uuid::new_v4()),
            sender: SENDER,
            origin_ts: BASE,
            content: Some(json!({ "membership": "join", "displayname": name })),
        })
        .await
        .expect("member state");
}

async fn preview_of(store: &Store, account_id: Uuid, room_id: &str) -> Option<RoomPreview> {
    let mut previews = store
        .room_previews(&[(account_id, room_id.to_owned())])
        .await
        .expect("room previews");
    assert!(previews.len() <= 1, "one room, at most one preview");
    previews.pop()
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn newest_message_with_sender_display_name() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "preview-basic").await;
    let room_id = new_room();

    insert_message(&store, account_id, &room_id, BASE, "older").await;
    let newest = insert_message(&store, account_id, &room_id, BASE + 1, "newest").await;
    set_display_name(&store, account_id, &room_id, json!("Alice")).await;

    let preview = preview_of(&store, account_id, &room_id)
        .await
        .expect("preview");
    assert_eq!(
        preview,
        RoomPreview {
            account_id,
            room_id: room_id.clone(),
            event_id: newest,
            sender: SENDER.to_owned(),
            sender_display_name: Some("Alice".to_owned()),
            origin_ts: BASE + 1,
            msgtype: Some("m.text".to_owned()),
            body: "newest".to_owned(),
            truncated: false,
        }
    );

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn display_name_is_null_without_a_string_displayname() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "preview-noname").await;
    let without_state = new_room();
    let non_string = new_room();

    insert_message(&store, account_id, &without_state, BASE, "hi").await;
    insert_message(&store, account_id, &non_string, BASE, "hi").await;
    // `MemberDto` reads `displayname` only when it is a string; so does this.
    set_display_name(&store, account_id, &non_string, json!(42)).await;

    for room in [&without_state, &non_string] {
        let preview = preview_of(&store, account_id, room).await.expect("preview");
        assert_eq!(preview.sender_display_name, None, "{room}");
    }

    common::cleanup_account(&pool, account_id).await;
}

/// Everything newer than the message is something the timeline does not show
/// as a row with a body, so the preview reaches past all of it.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn skips_events_without_a_previewable_body() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "preview-skip").await;
    let room_id = new_room();

    let message = insert_message(&store, account_id, &room_id, BASE, "the message").await;
    let mut ts = BASE;
    let mut next = || {
        ts += 1;
        ts
    };
    // A reaction, folded into its target's tally.
    insert_event(
        &store,
        account_id,
        &room_id,
        next(),
        "m.reaction",
        json!({}),
        None,
        Some(json!({ "rel_type": "m.annotation", "event_id": message, "key": "👍" })),
        None,
    )
    .await;
    // A membership change: a state event.
    insert_event(
        &store,
        account_id,
        &room_id,
        next(),
        "m.room.member",
        json!({ "membership": "join" }),
        Some("@bob:localhost"),
        None,
        None,
    )
    .await;
    // An undecryptable event: no content, no body.
    store
        .upsert_event(&NewEvent {
            event_id: &format!("$utd-{}:localhost", Uuid::new_v4()),
            room_id: &room_id,
            account_id,
            sender: SENDER,
            origin_ts: next(),
            event_type: "m.room.encrypted",
            content: None,
            raw_event: json!({ "type": "m.room.encrypted" }),
            megolm_session_id: Some("session"),
            redacts: None,
            relates_to: None,
            decrypted_body_text: None,
        })
        .await
        .expect("utd");
    // A message whose body is only whitespace.
    insert_message(&store, account_id, &room_id, next(), " \n\t ").await;
    // A message-typed state event.
    insert_event(
        &store,
        account_id,
        &room_id,
        next(),
        "m.room.message",
        json!({ "msgtype": "m.text", "body": "state, not a message" }),
        Some(""),
        None,
        None,
    )
    .await;
    // A sticker: not an `m.room.message`.
    insert_event(
        &store,
        account_id,
        &room_id,
        next(),
        "m.sticker",
        json!({ "body": "a sticker" }),
        None,
        None,
        None,
    )
    .await;

    let preview = preview_of(&store, account_id, &room_id)
        .await
        .expect("preview");
    assert_eq!(preview.event_id, message);
    assert_eq!(preview.body, "the message");

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn shows_the_latest_edit() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "preview-edit").await;
    let room_id = new_room();

    let original = insert_message(&store, account_id, &room_id, BASE, "typo").await;
    for (ts, body) in [(BASE + 1, "first fix"), (BASE + 2, "final")] {
        insert_event(
            &store,
            account_id,
            &room_id,
            ts,
            "m.room.message",
            json!({
                "msgtype": "m.text",
                "body": format!("* {body}"),
                "m.new_content": { "msgtype": "m.text", "body": body },
                "m.relates_to": { "rel_type": "m.replace", "event_id": original },
            }),
            None,
            Some(json!({ "rel_type": "m.replace", "event_id": original })),
            None,
        )
        .await;
    }

    let preview = preview_of(&store, account_id, &room_id)
        .await
        .expect("preview");
    // The edits are not rows of their own; the original shows the newest one.
    assert_eq!(preview.event_id, original);
    assert_eq!(preview.body, "final");
    assert_eq!(preview.origin_ts, BASE);

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_redacted_message_falls_back_to_the_one_before() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "preview-redact").await;
    let room_id = new_room();

    let kept = insert_message(&store, account_id, &room_id, BASE, "kept").await;
    let doomed = insert_message(&store, account_id, &room_id, BASE + 1, "doomed").await;
    // Same-server sender: the redaction applies without power-level evidence.
    insert_event(
        &store,
        account_id,
        &room_id,
        BASE + 2,
        "m.room.redaction",
        json!({}),
        None,
        None,
        Some(&doomed),
    )
    .await;

    let preview = preview_of(&store, account_id, &room_id)
        .await
        .expect("preview");
    assert_eq!(preview.event_id, kept);

    common::cleanup_account(&pool, account_id).await;
}

/// A thread reply is a row of its own in the room timeline, so it is the
/// preview when it is newest, as it was when the client scanned the timeline.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_thread_reply_is_previewed() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "preview-thread").await;
    let room_id = new_room();

    let root = insert_message(&store, account_id, &room_id, BASE, "root").await;
    let reply = insert_event(
        &store,
        account_id,
        &room_id,
        BASE + 1,
        "m.room.message",
        json!({ "msgtype": "m.text", "body": "in the thread" }),
        None,
        Some(json!({ "rel_type": "m.thread", "event_id": root })),
        None,
    )
    .await;

    let preview = preview_of(&store, account_id, &room_id)
        .await
        .expect("preview");
    assert_eq!(preview.event_id, reply);

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn looks_back_exactly_the_scan_limit() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "preview-scan").await;
    let at_limit = new_room();
    let past_limit = new_room();

    // In `at_limit` the message is the oldest of exactly the scan limit's
    // rows; in `past_limit` one more state event pushes it out of reach.
    for (room, filler) in [
        (&at_limit, ROOM_PREVIEW_SCAN_LIMIT - 1),
        (&past_limit, ROOM_PREVIEW_SCAN_LIMIT),
    ] {
        insert_message(&store, account_id, room, BASE, "deep").await;
        for i in 0..filler {
            insert_event(
                &store,
                account_id,
                room,
                BASE + 1 + i,
                "m.room.member",
                json!({ "membership": "join" }),
                Some(&format!("@user{i}:localhost")),
                None,
                None,
            )
            .await;
        }
    }

    let found = preview_of(&store, account_id, &at_limit).await;
    assert_eq!(found.map(|p| p.body).as_deref(), Some("deep"));
    assert_eq!(preview_of(&store, account_id, &past_limit).await, None);

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn cuts_long_bodies_by_character() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "preview-cut").await;
    let long_room = new_room();
    let exact_room = new_room();
    let cap = usize::try_from(ROOM_PREVIEW_BODY_CHARS).unwrap();

    // Multi-byte characters: the cap counts characters, not bytes.
    let long = "é".repeat(cap + 10);
    let exact = "é".repeat(cap);
    insert_message(&store, account_id, &long_room, BASE, &long).await;
    insert_message(&store, account_id, &exact_room, BASE, &exact).await;

    let cut = preview_of(&store, account_id, &long_room)
        .await
        .expect("preview");
    assert_eq!(cut.body, "é".repeat(cap));
    assert!(cut.truncated);
    let whole = preview_of(&store, account_id, &exact_room)
        .await
        .expect("preview");
    assert_eq!(whole.body, exact);
    assert!(!whole.truncated);

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn many_rooms_across_accounts_in_request_order() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let first = common::test_account(&store, "preview-many-a").await;
    let second = common::test_account(&store, "preview-many-b").await;
    let shared = new_room();
    let empty = new_room();
    let only_second = new_room();

    insert_message(&store, first, &shared, BASE, "first's view").await;
    insert_message(&store, second, &shared, BASE, "second's view").await;
    insert_message(&store, second, &only_second, BASE, "only second").await;
    // A room with no previewable message at all.
    insert_event(
        &store,
        first,
        &empty,
        BASE,
        "m.room.create",
        json!({ "room_version": "12" }),
        Some(""),
        None,
        None,
    )
    .await;

    let request = vec![
        (second, only_second.clone()),
        (first, empty.clone()),
        (first, shared.clone()),
        // The other account does not see this room's events.
        (first, only_second.clone()),
        (second, shared.clone()),
        (Uuid::new_v4(), shared.clone()),
    ];
    let previews = store.room_previews(&request).await.expect("previews");
    let got: Vec<(Uuid, &str, &str)> = previews
        .iter()
        .map(|p| (p.account_id, p.room_id.as_str(), p.body.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            (second, only_second.as_str(), "only second"),
            (first, shared.as_str(), "first's view"),
            (second, shared.as_str(), "second's view"),
        ]
    );
    assert!(store.room_previews(&[]).await.expect("empty").is_empty());

    common::cleanup_account(&pool, first).await;
    common::cleanup_account(&pool, second).await;
}

/// Rows the timeline folds away (reactions, redaction events, edits) are not
/// rows of the timeline, so they do not use up the scan: the message under
/// any number of them is still found, as it was by the client's page reads.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn folded_rows_do_not_use_up_the_scan() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "preview-folded").await;

    // One room per kind, so each strip in the query is pinned on its own.
    for kind in ["reaction", "redaction", "edit"] {
        let room_id = new_room();
        let message = insert_message(&store, account_id, &room_id, BASE, "under it all").await;
        for i in 0..ROOM_PREVIEW_SCAN_LIMIT {
            let ts = BASE + 1 + i;
            // Each points at an event that does not exist, so none of them
            // changes what the message shows.
            let gone = format!("$gone-{i}:localhost");
            match kind {
                "reaction" => {
                    let relation =
                        json!({ "rel_type": "m.annotation", "event_id": gone, "key": "👍" });
                    insert_event(
                        &store,
                        account_id,
                        &room_id,
                        ts,
                        "m.reaction",
                        json!({}),
                        None,
                        Some(relation),
                        None,
                    )
                    .await
                }
                "redaction" => {
                    insert_event(
                        &store,
                        account_id,
                        &room_id,
                        ts,
                        "m.room.redaction",
                        json!({}),
                        None,
                        None,
                        Some(&gone),
                    )
                    .await
                }
                _ => {
                    let relation = json!({ "rel_type": "m.replace", "event_id": gone });
                    let content = json!({
                        "msgtype": "m.text",
                        "body": "* edit",
                        "m.new_content": { "msgtype": "m.text", "body": "edit" },
                    });
                    insert_event(
                        &store,
                        account_id,
                        &room_id,
                        ts,
                        "m.room.message",
                        content,
                        None,
                        Some(relation),
                        None,
                    )
                    .await
                }
            };
        }
        let preview = preview_of(&store, account_id, &room_id).await;
        assert_eq!(preview.map(|p| p.event_id), Some(message), "{kind}");
    }

    common::cleanup_account(&pool, account_id).await;
}

/// Previews read on the hot-read pool (ADR 0112), as the timeline reads they
/// replace do: with the ordinary pool's only connection taken, as a burst of
/// auth checks or sync writes could take it, a preview read still completes.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn previews_read_on_the_read_pool_not_the_ordinary_pool() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let store = Store::connect(&url, 1).await.expect("connect + migrate");
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "preview-pool").await;
    let room_id = new_room();
    insert_message(&store, account_id, &room_id, BASE, "still readable").await;

    let held = store
        .pool()
        .acquire()
        .await
        .expect("hold the ordinary pool");
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        store.room_previews(&[(account_id, room_id.clone())]),
    )
    .await
    .expect("a preview read must not wait on the ordinary pool")
    .expect("previews");
    assert_eq!(read.len(), 1);
    drop(held);

    common::cleanup_account(&pool, account_id).await;
}
