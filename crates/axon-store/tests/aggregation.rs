//! DB-gated integration tests for M8 relation aggregation: edit collapse,
//! reaction tallies, replies, and threads — including the resolution rules
//! (authorship, redaction, dedup, deterministic tie-break) and the load-bearing
//! claim that aggregation resolves over relations *outside* the timeline window.
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store --test aggregation -- --ignored
//! ```

mod common;

use axon_store::{NewEvent, Store};
use common::insert_message;
use serde_json::{json, Value};
use uuid::Uuid;

/// The original message sender used by `insert_message`. Valid edits must be
/// authored by this same user.
const SENDER: &str = "@alice:localhost";

/// Insert an event carrying an explicit `relates_to` (and sender), returning its
/// event_id. The body, if present in `content`, is lifted into
/// `decrypted_body_text` like the live ingestion path does.
#[allow(clippy::too_many_arguments)]
async fn insert_relation(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    sender: &str,
    origin_ts: i64,
    event_type: &str,
    content: Value,
    relates_to: Value,
) -> String {
    let event_id = format!("$evt-{}:localhost", Uuid::new_v4());
    let body = content
        .get("body")
        .and_then(|b| b.as_str())
        .map(str::to_owned);
    store
        .upsert_event(&NewEvent {
            event_id: &event_id,
            room_id,
            account_id,
            sender,
            origin_ts,
            event_type,
            content: Some(content.clone()),
            raw_event: json!({ "type": event_type, "content": content }),
            megolm_session_id: None,
            redacts: None,
            relates_to: Some(relates_to),
            decrypted_body_text: body.as_deref(),
        })
        .await
        .expect("insert relation");
    event_id
}

/// An `m.replace` edit of `target` with a new text body, authored by `sender`.
async fn insert_edit(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    sender: &str,
    origin_ts: i64,
    target: &str,
    new_body: &str,
) -> String {
    insert_relation(
        store,
        account_id,
        room_id,
        sender,
        origin_ts,
        "m.room.message",
        json!({
            "msgtype": "m.text",
            "body": format!("* {new_body}"),
            "m.new_content": { "msgtype": "m.text", "body": new_body },
        }),
        json!({ "rel_type": "m.replace", "event_id": target }),
    )
    .await
}

/// An `m.annotation` reaction to `target` with `key`, authored by `sender`.
async fn insert_reaction(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    sender: &str,
    origin_ts: i64,
    target: &str,
    key: &str,
) -> String {
    insert_relation(
        store,
        account_id,
        room_id,
        sender,
        origin_ts,
        "m.reaction",
        json!({}),
        json!({ "rel_type": "m.annotation", "event_id": target, "key": key }),
    )
    .await
}

/// A plain reply (no `rel_type`, nested `m.in_reply_to`) to `target`.
async fn insert_reply(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    origin_ts: i64,
    target: &str,
    body: &str,
) -> String {
    insert_relation(
        store,
        account_id,
        room_id,
        SENDER,
        origin_ts,
        "m.room.message",
        json!({ "msgtype": "m.text", "body": body }),
        json!({ "m.in_reply_to": { "event_id": target } }),
    )
    .await
}

/// A thread reply: `rel_type: m.thread` rooted at `root`, with the usual
/// `m.in_reply_to` fallback (so the test proves replies and threads don't bleed).
async fn insert_thread_reply(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    origin_ts: i64,
    root: &str,
    body: &str,
) -> String {
    insert_relation(
        store,
        account_id,
        room_id,
        SENDER,
        origin_ts,
        "m.room.message",
        json!({ "msgtype": "m.text", "body": body }),
        json!({
            "rel_type": "m.thread",
            "event_id": root,
            "m.in_reply_to": { "event_id": root, "is_falling_back": true },
        }),
    )
    .await
}

/// Insert a state event (carries a `state_key`, so the resolver treats it as
/// non-replaceable per MSC2676). Returns its event_id.
async fn insert_state_event(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    origin_ts: i64,
    event_type: &str,
    content: Value,
) -> String {
    let event_id = format!("$state-{}:localhost", Uuid::new_v4());
    store
        .upsert_event(&NewEvent {
            event_id: &event_id,
            room_id,
            account_id,
            sender: SENDER,
            origin_ts,
            event_type,
            content: Some(content.clone()),
            raw_event: json!({ "type": event_type, "state_key": "", "content": content }),
            megolm_session_id: None,
            redacts: None,
            relates_to: None,
            decrypted_body_text: None,
        })
        .await
        .expect("insert state event");
    event_id
}

/// Redact `target` (an `m.room.redaction` pointing at it).
async fn redact(store: &Store, account_id: Uuid, room_id: &str, origin_ts: i64, target: &str) {
    let event_id = format!("$red-{}:localhost", Uuid::new_v4());
    store
        .upsert_event(&NewEvent {
            event_id: &event_id,
            room_id,
            account_id,
            sender: SENDER,
            origin_ts,
            event_type: "m.room.redaction",
            content: Some(json!({})),
            raw_event: json!({ "type": "m.room.redaction", "redacts": target }),
            megolm_session_id: None,
            redacts: Some(target),
            relates_to: None,
            decrypted_body_text: None,
        })
        .await
        .expect("insert redaction");
}

/// An account whose `user_id` is known, so tests can assert the reaction `me`
/// flag against the account's own user.
async fn account_with_user(store: &Store, prefix: &str) -> (Uuid, String) {
    let user = format!("@{prefix}-{}:localhost", Uuid::new_v4());
    let account = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("account");
    (account.account_id, account.user_id)
}

/// The headline fix (GH issue #22): a reaction and an edit that land *after* the
/// original scrolled out of the default window still resolve. The collapsed
/// timeline strips the standalone edit/reaction rows; the target shows the edited
/// body and reaction tally; the dedicated reads return the full picture.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn edit_and_reaction_resolve_outside_window() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "agg-win").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());

    let base = 1_700_000_000_000;
    let target = insert_message(&store, account_id, &room_id, base, "original").await;
    // A few later messages so the original isn't the newest thing in the room.
    for i in 1..=3 {
        insert_message(
            &store,
            account_id,
            &room_id,
            base + i,
            &format!("later {i}"),
        )
        .await;
    }
    // The edit and reaction arrive much later — "outside the window".
    insert_edit(
        &store,
        account_id,
        &room_id,
        SENDER,
        base + 100,
        &target,
        "edited!",
    )
    .await;
    insert_reaction(
        &store,
        account_id,
        &room_id,
        "@bob:localhost",
        base + 101,
        &target,
        "👍",
    )
    .await;

    // The collapsed timeline excludes the standalone edit and reaction events.
    let timeline = store
        .room_timeline(account_id, &room_id, None, 50)
        .await
        .expect("timeline");
    assert!(
        timeline.iter().all(|r| r.event_type != "m.reaction"),
        "reaction events must be stripped from the timeline"
    );
    assert!(
        timeline
            .iter()
            .all(|r| r.relates_to.as_ref().and_then(|v| v.get("rel_type"))
                != Some(&json!("m.replace"))),
        "edit events must be stripped from the timeline"
    );

    // The target — fetched directly — shows the resolved edited body + tally.
    let got = store
        .get_event(account_id, &target)
        .await
        .expect("get_event")
        .expect("target present");
    assert!(got.edited, "target should report edited");
    assert_eq!(got.edit_count, 1);
    assert_eq!(got.latest_edit_ts, Some(base + 100));
    assert_eq!(got.decrypted_body_text.as_deref(), Some("edited!"));
    assert_eq!(
        got.content.as_ref().and_then(|c| c.get("body")),
        Some(&json!("edited!")),
        "content body should be the latest edit"
    );
    let reactions = got.reactions.expect("reactions present");
    assert_eq!(reactions["👍"]["count"], json!(1));

    // The dedicated reaction read agrees, regardless of window.
    let tally = store
        .event_reactions(account_id, &target)
        .await
        .expect("reactions");
    assert_eq!(tally.len(), 1);
    assert_eq!(tally[0].key, "👍");
    assert_eq!(tally[0].count, 1);
    assert_eq!(tally[0].senders, vec!["@bob:localhost".to_string()]);
    assert!(!tally[0].me, "reaction was from @bob, not this account");

    common::cleanup_account(&pool, account_id).await;
}

/// Edit resolution rules: a non-sender edit is ignored; the latest *valid* edit
/// wins; ties on `origin_ts` break deterministically by `id`; a redacted edit
/// reverts to the prior body; an incompatible `msgtype` is dropped.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn edit_resolution_rules() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "agg-edit").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());
    let base = 1_700_000_000_000;

    // Non-sender edit is ignored.
    let t1 = insert_message(&store, account_id, &room_id, base, "v1").await;
    insert_edit(
        &store,
        account_id,
        &room_id,
        "@mallory:localhost",
        base + 1,
        &t1,
        "spoofed",
    )
    .await;
    let r1 = store.get_event(account_id, &t1).await.unwrap().unwrap();
    assert!(!r1.edited, "edit from a non-sender must be ignored");
    assert_eq!(r1.decrypted_body_text.as_deref(), Some("v1"));
    assert_eq!(r1.edit_count, 0);

    // Two edits at equal origin_ts: the later-inserted (higher id) wins; both count.
    let t2 = insert_message(&store, account_id, &room_id, base, "orig").await;
    insert_edit(&store, account_id, &room_id, SENDER, base + 5, &t2, "first").await;
    insert_edit(
        &store,
        account_id,
        &room_id,
        SENDER,
        base + 5,
        &t2,
        "second",
    )
    .await;
    let r2 = store.get_event(account_id, &t2).await.unwrap().unwrap();
    assert_eq!(r2.decrypted_body_text.as_deref(), Some("second"));
    assert_eq!(r2.edit_count, 2);
    assert_eq!(r2.latest_edit_ts, Some(base + 5));

    // A redacted edit stops contributing — body reverts to the prior text.
    let t3 = insert_message(&store, account_id, &room_id, base, "keep").await;
    let edit3 = insert_edit(
        &store,
        account_id,
        &room_id,
        SENDER,
        base + 1,
        &t3,
        "throwaway",
    )
    .await;
    redact(&store, account_id, &room_id, base + 2, &edit3).await;
    let r3 = store.get_event(account_id, &t3).await.unwrap().unwrap();
    assert!(!r3.edited, "a redacted edit must not contribute");
    assert_eq!(r3.decrypted_body_text.as_deref(), Some("keep"));
    assert_eq!(r3.edit_count, 0);

    // An edit whose replacement msgtype differs from the target is dropped.
    let t4 = insert_message(&store, account_id, &room_id, base, "text").await;
    insert_relation(
        &store,
        account_id,
        &room_id,
        SENDER,
        base + 1,
        "m.room.message",
        json!({
            "msgtype": "m.text",
            "body": "* notice",
            "m.new_content": { "msgtype": "m.notice", "body": "notice" },
        }),
        json!({ "rel_type": "m.replace", "event_id": t4 }),
    )
    .await;
    let r4 = store.get_event(account_id, &t4).await.unwrap().unwrap();
    assert!(!r4.edited, "an msgtype-incompatible edit must be dropped");
    assert_eq!(r4.decrypted_body_text.as_deref(), Some("text"));

    common::cleanup_account(&pool, account_id).await;
}

/// Reaction rules: `(sender, key)` deduplicates, a redacted reaction drops from
/// the tally, distinct emoji tally separately, and `me` reflects the account's
/// own user.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn reaction_tally_rules() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let (account_id, me_user) = account_with_user(&store, "agg-react").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());
    let base = 1_700_000_000_000;

    let target = insert_message(&store, account_id, &room_id, base, "react to me").await;

    // @bob reacts 👍 twice (duplicate) — counts once. This account reacts 👍 too.
    insert_reaction(
        &store,
        account_id,
        &room_id,
        "@bob:localhost",
        base + 1,
        &target,
        "👍",
    )
    .await;
    insert_reaction(
        &store,
        account_id,
        &room_id,
        "@bob:localhost",
        base + 2,
        &target,
        "👍",
    )
    .await;
    let my_thumb = insert_reaction(
        &store,
        account_id,
        &room_id,
        &me_user,
        base + 3,
        &target,
        "👍",
    )
    .await;
    // A ❤️ reaction that then gets redacted — drops from the tally entirely.
    let heart = insert_reaction(
        &store,
        account_id,
        &room_id,
        "@bob:localhost",
        base + 4,
        &target,
        "❤️",
    )
    .await;
    redact(&store, account_id, &room_id, base + 5, &heart).await;

    let tally = store
        .event_reactions(account_id, &target)
        .await
        .expect("reactions");
    assert_eq!(tally.len(), 1, "redacted ❤️ should leave the tally");
    let thumbs = &tally[0];
    assert_eq!(thumbs.key, "👍");
    assert_eq!(
        thumbs.count, 2,
        "@bob's duplicate counts once; +this account = 2"
    );
    assert!(thumbs.me, "this account reacted, so me=true");
    assert!(thumbs.senders.contains(&"@bob:localhost".to_string()));
    assert!(thumbs.senders.contains(&me_user));
    // The account's own reaction id survives @bob's duplicates (dedup happens
    // per (sender, key) before the cap, so one sender can't evict another's data).
    assert_eq!(
        thumbs.my_event_ids,
        vec![my_thumb],
        "my_event_ids is the account's own reaction id, for unreact"
    );

    common::cleanup_account(&pool, account_id).await;
}

/// Replies are matched on the nested `m.in_reply_to` target and are kept distinct
/// from thread members (which carry a `rel_type` even with the reply fallback).
#[tokio::test]
#[ignore = "requires Postgres"]
async fn replies_resolve_and_exclude_threads() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "agg-reply").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());
    let base = 1_700_000_000_000;

    let target = insert_message(&store, account_id, &room_id, base, "question").await;
    let reply = insert_reply(&store, account_id, &room_id, base + 1, &target, "answer").await;
    // A thread member that *also* nests m.in_reply_to at the same target — must
    // NOT be reported as a plain reply (it has a rel_type).
    insert_thread_reply(&store, account_id, &room_id, base + 2, &target, "in thread").await;

    let replies = store
        .event_replies(account_id, &target)
        .await
        .expect("replies");
    assert_eq!(replies.len(), 1, "thread member must not count as a reply");
    assert_eq!(replies[0].event_id, reply);
    assert_eq!(replies[0].decrypted_body_text.as_deref(), Some("answer"));

    common::cleanup_account(&pool, account_id).await;
}

/// Thread summaries and the thread-scoped timeline: `reply_count` excludes a
/// redacted member, but the scoped timeline still surfaces it (masked) for
/// structural continuity — pagination walks the full member list.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn threads_summarize_and_paginate() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "agg-thread").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());
    let base = 1_700_000_000_000;

    let root = insert_message(&store, account_id, &room_id, base, "thread root").await;
    let m1 = insert_thread_reply(&store, account_id, &room_id, base + 1, &root, "reply 1").await;
    let m2 = insert_thread_reply(&store, account_id, &room_id, base + 2, &root, "reply 2").await;
    let m3 = insert_thread_reply(&store, account_id, &room_id, base + 3, &root, "reply 3").await;
    // Redact the latest member: no longer a real message, so it drops out of the count.
    redact(&store, account_id, &room_id, base + 4, &m3).await;

    // A second, unrelated thread so we know `room_threads` groups by root.
    let root2 = insert_message(&store, account_id, &room_id, base + 10, "other root").await;
    insert_thread_reply(
        &store,
        account_id,
        &room_id,
        base + 11,
        &root2,
        "other reply",
    )
    .await;

    let threads = store
        .room_threads(account_id, &room_id)
        .await
        .expect("threads");
    assert_eq!(threads.len(), 2, "two distinct thread roots");
    // Most-recent-activity first: root2's member is base+11, root's latest is base+3.
    assert_eq!(threads[0].root_event_id, root2);
    let first = threads
        .iter()
        .find(|t| t.root_event_id == root)
        .expect("root thread");
    assert_eq!(
        first.reply_count, 2,
        "the redacted member no longer counts as a reply"
    );
    assert_eq!(first.latest_reply_event_id.as_deref(), Some(m2.as_str()));
    assert_eq!(first.latest_reply_ts, Some(base + 2));

    // The thread-scoped timeline returns only this thread's members, newest first.
    let page = store
        .thread_timeline(account_id, &room_id, &root, None, 50)
        .await
        .expect("thread timeline");
    let ids: Vec<&str> = page.iter().map(|r| r.event_id.as_str()).collect();
    assert_eq!(ids.len(), 3, "only this thread's members, root excluded");
    assert_eq!(ids[0], m3, "newest member first");
    // The redacted member is masked but present.
    let masked = page
        .iter()
        .find(|r| r.event_id == m3)
        .expect("redacted member present");
    assert!(masked.content.is_none(), "redacted member content masked");
    assert!(masked.redaction_event_id.is_some());

    // Pagination is stable across a page boundary inside the thread.
    let p1 = store
        .thread_timeline(account_id, &room_id, &root, None, 2)
        .await
        .expect("page 1");
    assert_eq!(p1.len(), 2);
    assert_eq!(p1[1].event_id, m2, "page 1 is the two newest members");
    let p2 = store
        .thread_timeline(
            account_id,
            &room_id,
            &root,
            Some(p1.last().unwrap().cursor()),
            2,
        )
        .await
        .expect("page 2");
    assert_eq!(p2.len(), 1);
    assert_eq!(p2[0].event_id, m1, "oldest member last, no overlap/skip");

    common::cleanup_account(&pool, account_id).await;
}

/// A state event can't legitimately carry an `m.thread` relation, but a hostile
/// or buggy client could send one; `room_threads` must not let it inflate the
/// reply count.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn thread_reply_count_excludes_state_events() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "agg-thread-state").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());
    let base = 1_700_000_000_000;

    let root = insert_message(&store, account_id, &room_id, base, "thread root").await;
    let m1 = insert_thread_reply(&store, account_id, &room_id, base + 1, &root, "reply 1").await;

    let state_member = format!("$state-{}:localhost", Uuid::new_v4());
    store
        .upsert_event(&NewEvent {
            event_id: &state_member,
            room_id: &room_id,
            account_id,
            sender: SENDER,
            origin_ts: base + 2,
            event_type: "m.room.topic",
            content: Some(json!({ "topic": "not a reply" })),
            raw_event: json!({
                "type": "m.room.topic",
                "state_key": "",
                "content": { "topic": "not a reply" },
            }),
            megolm_session_id: None,
            redacts: None,
            relates_to: Some(json!({ "rel_type": "m.thread", "event_id": root })),
            decrypted_body_text: None,
        })
        .await
        .expect("insert state-event thread member");

    let threads = store
        .room_threads(account_id, &room_id)
        .await
        .expect("threads");
    assert_eq!(threads.len(), 1);
    assert_eq!(
        threads[0].reply_count, 1,
        "the state-event member doesn't count as a reply"
    );
    assert_eq!(
        threads[0].latest_reply_event_id.as_deref(),
        Some(m1.as_str())
    );
    assert_eq!(threads[0].latest_reply_ts, Some(base + 1));

    common::cleanup_account(&pool, account_id).await;
}

/// Relations are room-local: an edit or reaction stored in room B must not
/// resolve onto a target with the same `event_id` in room A. (Regression for the
/// missing `room_id` scoping in the relation lookups.)
#[tokio::test]
#[ignore = "requires Postgres"]
async fn relations_are_room_local() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let (account_id, _me) = account_with_user(&store, "agg-room").await;
    let room_a = format!("!room-a-{}:localhost", Uuid::new_v4());
    let room_b = format!("!room-b-{}:localhost", Uuid::new_v4());
    let base = 1_700_000_000_000;

    let target = insert_message(&store, account_id, &room_a, base, "in room A").await;
    // An edit and a reaction to the *same target event_id* but stored in room B.
    insert_edit(
        &store,
        account_id,
        &room_b,
        SENDER,
        base + 1,
        &target,
        "cross-room edit",
    )
    .await;
    insert_reaction(
        &store,
        account_id,
        &room_b,
        "@bob:localhost",
        base + 2,
        &target,
        "👍",
    )
    .await;

    let got = store.get_event(account_id, &target).await.unwrap().unwrap();
    assert!(!got.edited, "a cross-room edit must not resolve");
    assert_eq!(got.edit_count, 0);
    assert_eq!(got.decrypted_body_text.as_deref(), Some("in room A"));
    assert!(
        got.reactions.is_none(),
        "a cross-room reaction must not tally"
    );

    assert!(
        store
            .event_edits(account_id, &target)
            .await
            .unwrap()
            .is_empty(),
        "event_edits is room-scoped"
    );
    assert!(
        store
            .event_reactions(account_id, &target)
            .await
            .unwrap()
            .is_empty(),
        "event_reactions is room-scoped"
    );

    common::cleanup_account(&pool, account_id).await;
}

/// Replacement validity per MSC2676: an edit must share the target's
/// `event_type`, neither may be a state event, and the target must not itself be
/// an `m.replace`. (Regression for the `com.example.edit` overwriting
/// `m.room.topic` case.)
#[tokio::test]
#[ignore = "requires Postgres"]
async fn replacement_validity_guards() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "agg-valid").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());
    let base = 1_700_000_000_000;

    // A state event (m.room.topic) "edited" by an arbitrary event type — both
    // msgtypes null, which the old `IS NOT DISTINCT FROM` check let through.
    let topic = insert_state_event(
        &store,
        account_id,
        &room_id,
        base,
        "m.room.topic",
        json!({ "topic": "original topic" }),
    )
    .await;
    insert_relation(
        &store,
        account_id,
        &room_id,
        SENDER,
        base + 1,
        "com.example.edit",
        json!({ "m.new_content": { "topic": "hijacked topic" } }),
        json!({ "rel_type": "m.replace", "event_id": topic }),
    )
    .await;
    let r_topic = store.get_event(account_id, &topic).await.unwrap().unwrap();
    assert!(!r_topic.edited, "a state event must not be replaceable");
    assert_eq!(r_topic.edit_count, 0);
    assert_eq!(
        r_topic.content.as_ref().and_then(|c| c.get("topic")),
        Some(&json!("original topic"))
    );

    // An edit whose own event_type differs from the (message) target is dropped.
    let msg = insert_message(&store, account_id, &room_id, base, "v1").await;
    insert_relation(
        &store,
        account_id,
        &room_id,
        SENDER,
        base + 1,
        "com.example.edit",
        json!({
            "msgtype": "m.text",
            "m.new_content": { "msgtype": "m.text", "body": "v2" },
        }),
        json!({ "rel_type": "m.replace", "event_id": msg }),
    )
    .await;
    let r_msg = store.get_event(account_id, &msg).await.unwrap().unwrap();
    assert!(
        !r_msg.edited,
        "an edit of a different event_type is dropped"
    );
    assert_eq!(r_msg.decrypted_body_text.as_deref(), Some("v1"));

    // A well-formed edit of the same type still works (control).
    let ok = insert_message(&store, account_id, &room_id, base, "before").await;
    insert_edit(&store, account_id, &room_id, SENDER, base + 1, &ok, "after").await;
    let r_ok = store.get_event(account_id, &ok).await.unwrap().unwrap();
    assert!(r_ok.edited, "a same-type message edit still resolves");
    assert_eq!(r_ok.decrypted_body_text.as_deref(), Some("after"));

    common::cleanup_account(&pool, account_id).await;
}

/// A redacted target masks its body *and* zeroes `edit_count` — the count must
/// not leak metadata behind a masked row. (Regression: `edited=false` but
/// `edit_count=1` previously.)
#[tokio::test]
#[ignore = "requires Postgres"]
async fn redacted_target_zeroes_edit_count() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "agg-redct").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());
    let base = 1_700_000_000_000;

    let target = insert_message(&store, account_id, &room_id, base, "doomed").await;
    insert_edit(
        &store,
        account_id,
        &room_id,
        SENDER,
        base + 1,
        &target,
        "edited",
    )
    .await;
    // Redact the *target* (not the edit). The valid edit still exists on disk.
    redact(&store, account_id, &room_id, base + 2, &target).await;

    let got = store.get_event(account_id, &target).await.unwrap().unwrap();
    assert!(!got.edited, "a redacted target reports unedited");
    assert_eq!(
        got.edit_count, 0,
        "edit_count must not leak behind redaction"
    );
    assert_eq!(got.latest_edit_ts, None);
    assert!(got.content.is_none(), "redacted content masked");
    assert!(got.redaction_event_id.is_some());

    common::cleanup_account(&pool, account_id).await;
}

/// The timeline collapses `m.reaction` annotations (folded into the per-event
/// tally) but *keeps* a non-`m.reaction` `m.annotation` as a raw row — it is
/// neither aggregated (GH #112) nor surfaced anywhere else, so collapsing it
/// would make it vanish. (Regression for the over-broad annotation strip.)
#[tokio::test]
#[ignore = "requires Postgres"]
async fn non_reaction_annotation_stays_in_timeline() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "agg-annot").await;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());
    let base = 1_700_000_000_000;

    let target = insert_message(&store, account_id, &room_id, base, "decision").await;
    // An emoji reaction — collapsed into the tally, stripped from the timeline.
    insert_reaction(
        &store,
        account_id,
        &room_id,
        "@bob:localhost",
        base + 1,
        &target,
        "👍",
    )
    .await;
    // A custom annotation event type — NOT aggregated, so must stay visible.
    let approval = insert_relation(
        &store,
        account_id,
        &room_id,
        "@bob:localhost",
        base + 2,
        "com.example.approval",
        json!({}),
        json!({ "rel_type": "m.annotation", "event_id": target, "key": "approve" }),
    )
    .await;

    let timeline = store
        .room_timeline(account_id, &room_id, None, 50)
        .await
        .expect("timeline");
    let ids: Vec<&str> = timeline.iter().map(|r| r.event_id.as_str()).collect();
    assert!(
        ids.contains(&approval.as_str()),
        "a non-m.reaction annotation must remain a raw timeline row"
    );
    assert!(
        timeline.iter().all(|r| r.event_type != "m.reaction"),
        "m.reaction annotations are still collapsed"
    );

    common::cleanup_account(&pool, account_id).await;
}
