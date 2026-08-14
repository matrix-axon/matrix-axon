//! Integration tests for the event store + re-decryption queue plumbing.
//!
//! These require a running Postgres and are `#[ignore]`d by default so the
//! normal `cargo test` stays database-free. Run them with:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store -- --ignored
//! ```

mod common;

use axon_store::{EventCiphertext, EventCrypto, NewEvent, RoomStateUpsert, TimelineCursor};
use common::insert_message;
use serde_json::{json, Value};
use sqlx_core::row::Row;
use sqlx_postgres::PgPool;
use uuid::Uuid;

async fn read_event(pool: &PgPool, account_id: Uuid, event_id: &str) -> (Option<Value>, String) {
    let row = sqlx_core::query::query(
        "SELECT content, event_type FROM events WHERE account_id = $1 AND event_id = $2",
    )
    .bind(account_id)
    .bind(event_id)
    .fetch_one(pool)
    .await
    .expect("read event");
    (
        row.try_get::<Option<Value>, _>("content").expect("content"),
        row.try_get::<String, _>("event_type").expect("event_type"),
    )
}

/// The full life cycle: a UTD lands with `content = NULL` and a session id, the
/// queue finds it, back-fills it, and the `content IS NULL` guard then makes the
/// update idempotent (no clobbering an already-decrypted row).
#[tokio::test]
#[ignore = "requires Postgres"]
async fn pending_utd_is_found_then_back_filled_idempotently() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;

    // Unique per run so repeated runs don't collide.
    let user = format!("@utd-{}:localhost", Uuid::new_v4());
    let account = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("upsert account");
    let account_id = account.account_id;

    let room_id = format!("!room-{}:localhost", Uuid::new_v4());
    let event_id = format!("$evt-{}:localhost", Uuid::new_v4());
    let session_id = format!("session-{}", Uuid::new_v4());

    // Insert a UTD by first constructing the raw_event with the session_id
    // (which is needed for the queue to find it) and then upserting with
    // `content = NULL` (which is needed for the back-fill guard).
    let raw_event = json!({
        "type": "m.room.encrypted",
        "event_id": event_id,
        "sender": "@alice:localhost",
        "content": { "algorithm": "m.megolm.v1.aes-sha2", "session_id": session_id }
    });
    store
        .upsert_event(&NewEvent {
            event_id: &event_id,
            room_id: &room_id,
            account_id,
            sender: "@alice:localhost",
            origin_ts: 1_700_000_000_000,
            event_type: "m.room.encrypted",
            content: None, // content is NULL for pending UTD
            raw_event: raw_event.clone(),
            megolm_session_id: Some(&session_id),
            redacts: None,
            relates_to: None,
            decrypted_body_text: None,
        })
        .await
        .expect("insert UTD");

    // The queue finds it by session...
    let by_session = store
        .pending_utds_for_session(account_id, &room_id, &session_id)
        .await
        .expect("pending by session");
    assert_eq!(by_session.len(), 1);
    assert_eq!(by_session[0].event_id, event_id);
    assert_eq!(by_session[0].room_id, room_id);
    assert_eq!(by_session[0].raw_event, raw_event);

    // ...and the startup sweep finds it among the account's backlog.
    let by_account = store
        .pending_utds_for_account(account_id)
        .await
        .expect("pending by account");
    assert!(by_account.iter().any(|p| p.event_id == event_id));
    let startup = store
        .pending_utds_for_startup_attempt(account_id)
        .await
        .expect("pending startup");
    assert!(startup.iter().any(|p| p.event_id == event_id));

    let marked = store
        .mark_utd_startup_redecrypt_attempted(account_id, std::slice::from_ref(&event_id))
        .await
        .expect("mark startup attempted");
    assert_eq!(marked, 1);
    assert!(store
        .pending_utds_for_startup_attempt(account_id)
        .await
        .expect("pending startup after mark")
        .is_empty());
    assert_eq!(
        store
            .pending_utds_for_session(account_id, &room_id, &session_id)
            .await
            .expect("pending by session after mark")
            .len(),
        1,
        "startup marker must not suppress fresh room-key arrival retries"
    );
    assert_eq!(
        store
            .pending_utds_for_account(account_id)
            .await
            .expect("all pending after mark")
            .len(),
        1,
        "manual/recovery retries must ignore the startup marker"
    );

    // Back-fill the decrypted payload.
    let content = json!({ "msgtype": "m.text", "body": "decrypted!" });
    store
        .update_decrypted_event(
            account_id,
            &event_id,
            &content,
            "m.room.message",
            Some("decrypted!"),
            None,
        )
        .await
        .expect("back-fill");

    // Row is no longer pending, and content/type were written.
    assert!(store
        .pending_utds_for_session(account_id, &room_id, &session_id)
        .await
        .expect("pending after flip")
        .is_empty());
    let (stored_content, stored_type) = read_event(&pool, account_id, &event_id).await;
    assert_eq!(stored_content, Some(content.clone()));
    assert_eq!(stored_type, "m.room.message");

    // Guard/idempotency: a second update can't clobber the decrypted row
    // (the `content IS NULL` guard no longer matches).
    store
        .update_decrypted_event(
            account_id,
            &event_id,
            &json!({ "body": "SHOULD NOT OVERWRITE" }),
            "m.room.redaction",
            Some("SHOULD NOT OVERWRITE"),
            None,
        )
        .await
        .expect("guarded update");
    let (after_content, after_type) = read_event(&pool, account_id, &event_id).await;
    assert_eq!(after_content, Some(content));
    assert_eq!(after_type, "m.room.message");

    // And re-delivering the original UTD envelope must not reset content to NULL
    // (upsert is ON CONFLICT DO NOTHING).
    store
        .upsert_event(&NewEvent {
            event_id: &event_id,
            room_id: &room_id,
            account_id,
            sender: "@alice:localhost",
            origin_ts: 1_700_000_000_000,
            event_type: "m.room.encrypted",
            content: None,
            raw_event,
            megolm_session_id: Some(&session_id),
            redacts: None,
            relates_to: None,
            decrypted_body_text: None,
        })
        .await
        .expect("re-upsert UTD");
    assert!(store
        .pending_utds_for_session(account_id, &room_id, &session_id)
        .await
        .expect("pending after re-upsert")
        .is_empty());

    common::cleanup_account(&pool, account_id).await;
}

/// Cursor pagination over a room timeline: pages are reverse-chronological,
/// non-overlapping, and stable across calls.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn room_timeline_paginates_reverse_chronologically() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;

    let user = format!("@tl-{}:localhost", Uuid::new_v4());
    let account_id = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("account")
        .account_id;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());

    // Five events, ascending in time.
    let base_ts = 1_700_000_000_000;
    for i in 0..5 {
        insert_message(
            &store,
            account_id,
            &room_id,
            base_ts + i,
            &format!("msg {i}"),
        )
        .await;
    }

    // Walk the timeline in pages of 2, newest first.
    let mut seen: Vec<i64> = Vec::new();
    let mut cursor: Option<TimelineCursor> = None;
    loop {
        let page = store
            .room_timeline(account_id, &room_id, cursor, 2)
            .await
            .expect("page");
        if page.is_empty() {
            break;
        }
        // Each page is strictly descending by origin_ts.
        for w in page.windows(2) {
            assert!(w[0].origin_ts > w[1].origin_ts, "page not descending");
        }
        cursor = Some(page.last().unwrap().cursor());
        seen.extend(page.iter().map(|r| r.origin_ts));
    }

    // All five, newest→oldest, no overlap or skips.
    assert_eq!(
        seen,
        vec![base_ts + 4, base_ts + 3, base_ts + 2, base_ts + 1, base_ts]
    );

    common::cleanup_account(&pool, account_id).await;
}

/// The `id` tiebreaker: events sharing one `origin_ts` paginate stably — ordered
/// by `id` descending, with no overlap or skip across a page boundary that falls
/// *inside* the tie group. (The reverse-chron test above uses distinct
/// timestamps, so it never actually exercises the tiebreaker.)
#[tokio::test]
#[ignore = "requires Postgres"]
async fn room_timeline_tiebreaks_same_timestamp_by_id() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;

    let user = format!("@tie-{}:localhost", Uuid::new_v4());
    let account_id = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("account")
        .account_id;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());

    // Four events sharing one origin_ts — only `id` distinguishes them. Inserts
    // get ascending BIGSERIAL ids, so insertion order == id order.
    let ts = 1_700_000_000_000;
    let mut ids = Vec::new();
    for i in 0..4 {
        ids.push(insert_message(&store, account_id, &room_id, ts, &format!("same-ts {i}")).await);
    }

    // Pages of 2 — the boundary lands inside the tie group.
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<TimelineCursor> = None;
    loop {
        let page = store
            .room_timeline(account_id, &room_id, cursor, 2)
            .await
            .expect("page");
        if page.is_empty() {
            break;
        }
        // All share origin_ts, so ordering is purely by id, descending.
        for w in page.windows(2) {
            assert_eq!(w[0].origin_ts, w[1].origin_ts);
            assert!(w[0].id > w[1].id, "tie group not ordered by id desc");
        }
        cursor = Some(page.last().unwrap().cursor());
        seen.extend(page.iter().map(|r| r.event_id.clone()));
    }

    // Every event exactly once, newest-inserted (highest id) first.
    let mut expected = ids.clone();
    expected.reverse();
    assert_eq!(seen, expected, "tiebroken order wrong or page overlap/skip");

    common::cleanup_account(&pool, account_id).await;
}

/// A redacted event is masked at read time — content/body cleared and
/// `redaction_event_id` set — while its ciphertext sibling survives untouched.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn room_timeline_masks_redacted_events() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;

    let user = format!("@red-{}:localhost", Uuid::new_v4());
    let account_id = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("account")
        .account_id;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());

    let target = insert_message(&store, account_id, &room_id, 1_700_000_000_000, "secret").await;

    // The target started as an encrypted event, so it has a ciphertext sibling.
    store
        .insert_event_ciphertext(&EventCiphertext {
            account_id,
            event_id: &target,
            room_id: &room_id,
            algorithm: "m.megolm.v1.aes-sha2",
            sender_key: Some("CURVE25519"),
            session_id: Some("session-xyz"),
            ciphertext: json!({ "algorithm": "m.megolm.v1.aes-sha2", "ciphertext": "BASE64" }),
        })
        .await
        .expect("ciphertext sibling");

    // A redaction event pointing at the target.
    let redaction_id = format!("$red-{}:localhost", Uuid::new_v4());
    store
        .upsert_event(&NewEvent {
            event_id: &redaction_id,
            room_id: &room_id,
            account_id,
            sender: "@alice:localhost",
            origin_ts: 1_700_000_000_500,
            event_type: "m.room.redaction",
            content: Some(json!({ "reason": "oops" })),
            raw_event: json!({ "type": "m.room.redaction", "redacts": target }),
            megolm_session_id: None,
            redacts: Some(&target),
            relates_to: None,
            decrypted_body_text: None,
        })
        .await
        .expect("insert redaction");

    let timeline = store
        .room_timeline(account_id, &room_id, None, 10)
        .await
        .expect("timeline");

    let masked = timeline
        .iter()
        .find(|r| r.event_id == target)
        .expect("target present");
    assert!(masked.content.is_none(), "content should be masked");
    assert!(
        masked.decrypted_body_text.is_none(),
        "body should be masked"
    );
    assert_eq!(
        masked.redaction_event_id.as_deref(),
        Some(redaction_id.as_str())
    );

    // The ciphertext sibling is untouched by the read-time masking.
    let ct_count: i64 = sqlx_core::query::query(
        "SELECT count(*) AS c FROM event_ciphertext WHERE account_id = $1 AND event_id = $2",
    )
    .bind(account_id)
    .bind(&target)
    .fetch_one(&pool)
    .await
    .expect("count ciphertext")
    .try_get("c")
    .expect("c");
    assert_eq!(ct_count, 1);

    common::cleanup_account(&pool, account_id).await;
}

/// A standalone `m.room.redaction` row is collapsed out of the page like an
/// `m.replace` edit or `m.reaction` annotation (M8): it carries no displayable
/// body of its own (just a `redacts` pointer), and its effect already surfaces
/// on the target row via `redaction_event_id`/masking, so leaving it un-collapsed
/// would just be a blank row — the bug bridges/bots hit when they redact-and-
/// repost instead of sending `m.replace` edits.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn room_timeline_collapses_standalone_redaction_events() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;

    let user = format!("@redcol-{}:localhost", Uuid::new_v4());
    let account_id = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("account")
        .account_id;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());

    let target = insert_message(&store, account_id, &room_id, 1_700_000_000_000, "secret").await;

    let redaction_id = format!("$red-{}:localhost", Uuid::new_v4());
    store
        .upsert_event(&NewEvent {
            event_id: &redaction_id,
            room_id: &room_id,
            account_id,
            sender: "@alice:localhost",
            origin_ts: 1_700_000_000_500,
            event_type: "m.room.redaction",
            content: Some(json!({ "reason": "oops" })),
            raw_event: json!({ "type": "m.room.redaction", "redacts": target }),
            megolm_session_id: None,
            redacts: Some(&target),
            relates_to: None,
            decrypted_body_text: None,
        })
        .await
        .expect("insert redaction");

    let timeline = store
        .room_timeline(account_id, &room_id, None, 10)
        .await
        .expect("timeline");

    assert!(
        timeline.iter().all(|r| r.event_id != redaction_id),
        "the redaction event itself must not appear as its own row"
    );
    assert!(
        timeline.iter().any(|r| r.event_id == target),
        "the redacted target row must still be present (masked)"
    );

    common::cleanup_account(&pool, account_id).await;
}

/// The crypto sibling tables upsert from EncryptionInfo and cascade-delete with
/// their event row.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn event_crypto_siblings_upsert_and_cascade() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;

    let user = format!("@cry-{}:localhost", Uuid::new_v4());
    let account_id = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("account")
        .account_id;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());
    let event_id = insert_message(&store, account_id, &room_id, 1_700_000_000_000, "hi").await;

    // Helper: read back the coarse state + the M7c verdict.
    let read_back = |account_id, event_id: String| {
        let pool = pool.clone();
        async move {
            let row = sqlx_core::query::query(
                "SELECT verification_state AS v, sender_trust AS t FROM event_sender_device_keys \
                 WHERE account_id = $1 AND event_id = $2",
            )
            .bind(account_id)
            .bind(&event_id)
            .fetch_one(&pool)
            .await
            .expect("device row");
            let state: String = row.try_get("v").expect("v");
            let trust: Option<String> = row.try_get("t").expect("t");
            (state, trust)
        }
    };

    // Initial write with no verdict yet (e.g. a UTD persisted before decryption).
    // The upsert returns the effective stored verdict — still NULL here.
    let returned = store
        .upsert_event_crypto(&EventCrypto {
            account_id,
            event_id: &event_id,
            session_id: Some("session-1"),
            curve25519_key: Some("CURVE"),
            ed25519_key: Some("ED"),
            forwarded: false,
            forwarder_user_id: None,
            forwarder_device_id: None,
            device_id: Some("DEVICEA"),
            verification_state: "unverified",
            sender_trust: None,
        })
        .await
        .expect("crypto insert");
    assert_eq!(returned, None, "no verdict recorded yet");

    // Re-decryption first records a verdict: a NULL snapshot is populated, and the
    // upsert returns the newly-stored verdict (the value a live frame would emit).
    let returned = store
        .upsert_event_crypto(&EventCrypto {
            account_id,
            event_id: &event_id,
            session_id: Some("session-1"),
            curve25519_key: Some("CURVE"),
            ed25519_key: Some("ED"),
            forwarded: false,
            forwarder_user_id: None,
            forwarder_device_id: None,
            device_id: Some("DEVICEA"),
            verification_state: "unverified",
            sender_trust: Some("unverified"),
        })
        .await
        .expect("crypto upsert (populate null)");
    assert_eq!(returned.as_deref(), Some("unverified"));
    let (state, trust) = read_back(account_id, event_id.clone()).await;
    assert_eq!(state, "unverified");
    assert_eq!(
        trust.as_deref(),
        Some("unverified"),
        "a NULL snapshot is populated by the first non-null verdict"
    );

    // A duplicate delivery whose *newly-derived* trust differs (the device is
    // verified afterwards) must NOT rewrite the immutable at-decrypt snapshot. The
    // coarse legacy `verification_state` still overwrites, but `sender_trust` is
    // frozen — and crucially the upsert RETURNS the frozen verdict, not the fresh
    // one, so the live `timeline.event` frame agrees with the persisted snapshot
    // and HTTP reads (ADR 0031).
    let returned = store
        .upsert_event_crypto(&EventCrypto {
            account_id,
            event_id: &event_id,
            session_id: Some("session-1"),
            curve25519_key: Some("CURVE"),
            ed25519_key: Some("ED"),
            forwarded: false,
            forwarder_user_id: None,
            forwarder_device_id: None,
            device_id: Some("DEVICEA"),
            verification_state: "verified",
            sender_trust: Some("verified"),
        })
        .await
        .expect("crypto upsert (freeze)");
    assert_eq!(
        returned.as_deref(),
        Some("unverified"),
        "the upsert returns the frozen verdict a live frame must emit, not the fresh one"
    );
    let (state, trust) = read_back(account_id, event_id.clone()).await;
    assert_eq!(
        state, "verified",
        "coarse verification_state still overwrites"
    );
    assert_eq!(
        trust.as_deref(),
        Some("unverified"),
        "the immutable sender_trust snapshot keeps its first non-null verdict"
    );

    // Deleting the event cascades to both sibling tables.
    sqlx_core::query::query("DELETE FROM events WHERE account_id = $1 AND event_id = $2")
        .bind(account_id)
        .bind(&event_id)
        .execute(&pool)
        .await
        .expect("delete event");
    for table in ["event_megolm_session", "event_sender_device_keys"] {
        let n: i64 = sqlx_core::query::query(&format!(
            "SELECT count(*) AS c FROM {table} WHERE account_id = $1 AND event_id = $2"
        ))
        .bind(account_id)
        .bind(&event_id)
        .fetch_one(&pool)
        .await
        .expect("count sibling")
        .try_get("c")
        .expect("c");
        assert_eq!(n, 0, "{table} should cascade-delete");
    }

    common::cleanup_account(&pool, account_id).await;
}

/// M7c: `event_sender_trust` reads the at-decrypt snapshot back — the sender is
/// always present (it's on `events`), the sibling fields appear once a crypto row
/// is written, and an unknown event is `None`.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn event_sender_trust_reads_snapshot() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;

    let user = format!("@trust-{}:localhost", Uuid::new_v4());
    let account_id = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .expect("account")
        .account_id;
    let room_id = format!("!room-{}:localhost", Uuid::new_v4());
    let event_id = insert_message(&store, account_id, &room_id, 1_700_000_000_000, "hi").await;

    // Before any crypto sibling: the row exists (sender set) but no verdict.
    let pre = store
        .event_sender_trust(account_id, &event_id)
        .await
        .expect("query")
        .expect("event exists");
    assert_eq!(pre.sender, "@alice:localhost");
    assert_eq!(pre.verification_state, None);
    assert_eq!(pre.sender_trust, None);
    assert_eq!(pre.session_id, None);
    assert_eq!(pre.forwarded, None);

    // After a crypto sibling with a verdict, the snapshot fields come back —
    // including the Megolm session provenance from the sibling session row.
    store
        .upsert_event_crypto(&EventCrypto {
            account_id,
            event_id: &event_id,
            session_id: Some("session-7"),
            curve25519_key: Some("CURVE"),
            ed25519_key: Some("ED"),
            forwarded: true,
            forwarder_user_id: Some("@carol:localhost"),
            forwarder_device_id: Some("CAROLDEVICE"),
            device_id: Some("BOBDEVICE"),
            verification_state: "unverified",
            sender_trust: Some("verification_violation"),
        })
        .await
        .expect("crypto insert");

    let post = store
        .event_sender_trust(account_id, &event_id)
        .await
        .expect("query")
        .expect("event exists");
    assert_eq!(post.device_id.as_deref(), Some("BOBDEVICE"));
    assert_eq!(post.verification_state.as_deref(), Some("unverified"));
    assert_eq!(post.sender_trust.as_deref(), Some("verification_violation"));
    // Megolm session provenance (the spec's content-authentication evidence).
    assert_eq!(post.session_id.as_deref(), Some("session-7"));
    assert_eq!(post.forwarded, Some(true));
    assert_eq!(post.forwarder_user_id.as_deref(), Some("@carol:localhost"));
    assert_eq!(post.forwarder_device_id.as_deref(), Some("CAROLDEVICE"));

    // An unknown event is None (a 404 at the API).
    let missing = store
        .event_sender_trust(account_id, "$nope:localhost")
        .await
        .expect("query");
    assert!(missing.is_none());

    common::cleanup_account(&pool, account_id).await;
}

// ── get_event_by_mxc_url ────────────────────────────────────────────────────

async fn insert_image_event(
    store: &axon_store::Store,
    account_id: Uuid,
    room_id: &str,
    content: serde_json::Value,
) -> String {
    insert_event_with_type(store, account_id, room_id, "m.room.message", content).await
}

async fn insert_event_with_type(
    store: &axon_store::Store,
    account_id: Uuid,
    room_id: &str,
    event_type: &str,
    content: serde_json::Value,
) -> String {
    let event_id = format!("$img-{}:localhost", Uuid::new_v4());
    store
        .upsert_event(&NewEvent {
            event_id: &event_id,
            room_id,
            account_id,
            sender: "@alice:localhost",
            origin_ts: 1_700_000_000_000,
            event_type,
            content: Some(content.clone()),
            raw_event: json!({ "type": event_type, "content": content }),
            megolm_session_id: None,
            redacts: None,
            relates_to: None,
            decrypted_body_text: None,
        })
        .await
        .expect("insert event");
    event_id
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn get_event_by_mxc_url_finds_plain_media() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "mxc-plain").await;
    let room_id = format!("!r-{}:localhost", Uuid::new_v4());
    let mxc = format!("mxc://example.org/{}", Uuid::new_v4().simple());

    let event_id = insert_image_event(
        &store,
        account_id,
        &room_id,
        json!({ "msgtype": "m.image", "body": "photo.jpg", "url": mxc }),
    )
    .await;

    let found = store
        .get_event_by_mxc_url(account_id, &mxc)
        .await
        .expect("lookup");
    assert!(found.is_some(), "should find plain-media event");
    assert_eq!(found.unwrap().event_id, event_id);

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn get_event_by_mxc_url_finds_encrypted_media() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "mxc-enc").await;
    let room_id = format!("!r-{}:localhost", Uuid::new_v4());
    let mxc = format!("mxc://example.org/{}", Uuid::new_v4().simple());

    // Encrypted images have no top-level `url`; the MXC lives in `file.url`.
    let content = json!({
        "msgtype": "m.image",
        "body": "secret.jpg",
        "file": {
            "url": mxc,
            "key": { "kty": "oct", "alg": "A256CTR", "k": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", "key_ops": ["encrypt","decrypt"], "ext": true },
            "iv": "AAAAAAAAAAAAAAAAAAAAAA==",
            "hashes": { "sha256": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=" },
            "v": "v2"
        }
    });
    let event_id = insert_image_event(&store, account_id, &room_id, content).await;

    let found = store
        .get_event_by_mxc_url(account_id, &mxc)
        .await
        .expect("lookup");
    assert!(found.is_some(), "should find encrypted-media event");
    let row = found.unwrap();
    assert_eq!(row.event_id, event_id);
    // Caller extracts content.file to pass to the media proxy for decryption.
    let file = row.content.unwrap();
    assert!(file.get("file").is_some(), "content.file should be present");

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn get_event_by_mxc_url_finds_plain_thumbnail() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "mxc-plain-thumb").await;
    let room_id = format!("!r-{}:localhost", Uuid::new_v4());
    let mxc = format!("mxc://example.org/{}", Uuid::new_v4().simple());

    let content = json!({
        "msgtype": "m.video",
        "body": "clip.mp4",
        "url": "mxc://example.org/plain-video",
        "info": { "thumbnail_url": mxc }
    });
    let event_id = insert_image_event(&store, account_id, &room_id, content).await;

    let found = store
        .get_event_by_mxc_url(account_id, &mxc)
        .await
        .expect("lookup")
        .expect("plain thumbnail event");
    assert_eq!(found.event_id, event_id);

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn get_event_by_mxc_url_finds_member_avatar() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "mxc-member-avatar").await;
    let room_id = format!("!r-{}:localhost", Uuid::new_v4());
    let mxc = format!("mxc://example.org/{}", Uuid::new_v4().simple());

    let event_id = insert_event_with_type(
        &store,
        account_id,
        &room_id,
        "m.room.member",
        json!({ "membership": "join", "avatar_url": mxc }),
    )
    .await;

    let found = store
        .get_event_by_mxc_url(account_id, &mxc)
        .await
        .expect("lookup")
        .expect("member avatar event");
    assert_eq!(found.event_id, event_id);

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn get_event_by_mxc_url_prefers_encrypted_media_over_member_avatar_collision() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "mxc-avatar-collision").await;
    let room_id = format!("!r-{}:localhost", Uuid::new_v4());
    let mxc = format!("mxc://example.org/{}", Uuid::new_v4().simple());

    insert_event_with_type(
        &store,
        account_id,
        &room_id,
        "m.room.member",
        json!({ "membership": "join", "avatar_url": mxc }),
    )
    .await;
    let media_event_id = insert_image_event(
        &store,
        account_id,
        &room_id,
        json!({
            "msgtype": "m.image",
            "body": "secret.jpg",
            "file": {
                "url": mxc,
                "key": { "kty": "oct", "alg": "A256CTR", "k": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", "key_ops": ["encrypt","decrypt"], "ext": true },
                "iv": "AAAAAAAAAAAAAAAAAAAAAA==",
                "hashes": { "sha256": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=" },
                "v": "v2"
            }
        }),
    )
    .await;

    let found = store
        .get_event_by_mxc_url(account_id, &mxc)
        .await
        .expect("lookup")
        .expect("encrypted media event");
    assert_eq!(found.event_id, media_event_id);
    assert!(
        found
            .content
            .as_ref()
            .and_then(|content| content.get("file"))
            .is_some(),
        "content.file should be preserved for encrypted media"
    );

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn get_event_by_mxc_url_finds_encrypted_thumbnail() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "mxc-enc-thumb").await;
    let room_id = format!("!r-{}:localhost", Uuid::new_v4());
    let mxc = format!("mxc://example.org/{}", Uuid::new_v4().simple());

    let content = json!({
        "msgtype": "m.video",
        "body": "clip.mp4",
        "url": "mxc://example.org/plain-video",
        "info": {
            "thumbnail_file": {
                "url": mxc,
                "key": { "kty": "oct", "alg": "A256CTR", "k": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", "key_ops": ["encrypt","decrypt"], "ext": true },
                "iv": "AAAAAAAAAAAAAAAAAAAAAA==",
                "hashes": { "sha256": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=" },
                "v": "v2"
            }
        }
    });
    let event_id = insert_image_event(&store, account_id, &room_id, content).await;

    let found = store
        .get_event_by_mxc_url(account_id, &mxc)
        .await
        .expect("lookup")
        .expect("encrypted thumbnail event");
    assert_eq!(found.event_id, event_id);
    assert_eq!(
        found
            .content
            .as_ref()
            .and_then(|content| content.pointer("/info/thumbnail_file/url"))
            .and_then(serde_json::Value::as_str),
        Some(mxc.as_str())
    );

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn get_event_by_mxc_url_returns_none_for_unknown_url() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "mxc-miss").await;

    let found = store
        .get_event_by_mxc_url(account_id, "mxc://nowhere.example/doesnotexist")
        .await
        .expect("lookup");
    assert!(found.is_none());

    common::cleanup_account(&pool, account_id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn get_event_by_mxc_url_is_scoped_to_account() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let owner = common::test_account(&store, "mxc-owner").await;
    let other = common::test_account(&store, "mxc-other").await;
    let room_id = format!("!r-{}:localhost", Uuid::new_v4());
    let mxc = format!("mxc://example.org/{}", Uuid::new_v4().simple());

    // Insert only under `owner`.
    insert_image_event(
        &store,
        owner,
        &room_id,
        json!({ "msgtype": "m.image", "body": "photo.jpg", "url": mxc }),
    )
    .await;

    // `other` must not see it.
    let found = store
        .get_event_by_mxc_url(other, &mxc)
        .await
        .expect("lookup");
    assert!(found.is_none(), "must not cross account boundaries");

    common::cleanup_account(&pool, owner).await;
    common::cleanup_account(&pool, other).await;
}

/// Set `m.room.pinned_events` for a room to the given pinned id list.
async fn set_pinned(store: &axon_store::Store, account_id: Uuid, room_id: &str, pinned: &[&str]) {
    store
        .upsert_room_state(&RoomStateUpsert {
            account_id,
            room_id,
            event_type: "m.room.pinned_events",
            state_key: "",
            event_id: &format!("$pin-{}:localhost", Uuid::new_v4()),
            sender: "@alice:localhost",
            origin_ts: 1,
            content: Some(json!({ "pinned": pinned })),
        })
        .await
        .expect("pinned_events state");
}

/// Pinned events are returned in the pinned list's own array order — not
/// `origin_ts` order, which is deliberately reversed here from the pin order —
/// and hydrated through the same timeline projection as any other read (issue
/// #404, ADR 0084).
#[tokio::test]
#[ignore = "requires Postgres"]
async fn pinned_events_resolves_in_pinned_list_order_and_hydrates_content() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "pinned").await;
    let room_id = format!("!r-{}:localhost", Uuid::new_v4());

    // Sent oldest-first (e1, e2) but pinned newest-first ([e2, e1]) so the
    // ordering assertion can't pass by accident via origin_ts.
    let e1 = insert_message(&store, account_id, &room_id, 1_000, "first").await;
    let e2 = insert_message(&store, account_id, &room_id, 2_000, "second").await;
    set_pinned(&store, account_id, &room_id, &[&e2, &e1]).await;

    let pinned = store
        .pinned_events(account_id, &room_id)
        .await
        .expect("pinned_events");
    assert_eq!(pinned.len(), 2);
    assert_eq!(pinned[0].event_id, e2);
    assert_eq!(pinned[0].decrypted_body_text.as_deref(), Some("second"));
    assert_eq!(pinned[1].event_id, e1);
    assert_eq!(pinned[1].decrypted_body_text.as_deref(), Some("first"));

    common::cleanup_account(&pool, account_id).await;
}

/// A pinned id with no matching event (never backfilled, or since purged) is
/// silently dropped rather than failing the whole read; a room with no
/// `m.room.pinned_events` state at all reads as an empty list.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn pinned_events_drops_missing_ids_and_is_empty_when_unset() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "pinned-gap").await;
    let room_id = format!("!r-{}:localhost", Uuid::new_v4());

    let unset = store
        .pinned_events(account_id, &room_id)
        .await
        .expect("pinned_events");
    assert!(unset.is_empty());

    let e1 = insert_message(&store, account_id, &room_id, 1_000, "kept").await;
    let missing = format!("$missing-{}:localhost", Uuid::new_v4());
    set_pinned(&store, account_id, &room_id, &[&missing, &e1]).await;

    let pinned = store
        .pinned_events(account_id, &room_id)
        .await
        .expect("pinned_events");
    assert_eq!(pinned.len(), 1);
    assert_eq!(pinned[0].event_id, e1);

    common::cleanup_account(&pool, account_id).await;
}

/// `upsert_event` reports the same arrival order for a re-delivered event.
///
/// The receipt target a client names is an `arrival_order` (ADR 0089), and the
/// live path takes it from this return value. Sync re-delivers events routinely
/// — a resumed `/sync`, a backfill overlapping the live tail — so if the
/// conflict fallback ever reported a different number (or the caller had to
/// invent one), the client would receipt a position that does not exist and the
/// room would not clear. Pins the "stable across re-delivery" half of the
/// contract, which the conflict-fallback `SELECT` is the only thing providing.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn upsert_event_reports_the_same_arrival_order_on_re_delivery() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "arrival-dup").await;
    let room_id = format!("!r-{}:localhost", Uuid::new_v4());
    let event_id = format!("$evt-{}:localhost", Uuid::new_v4());
    let content = json!({ "msgtype": "m.text", "body": "once" });
    let ev = NewEvent {
        event_id: &event_id,
        room_id: &room_id,
        account_id,
        sender: "@alice:localhost",
        origin_ts: 1_700_000_000_000,
        event_type: "m.room.message",
        content: Some(content.clone()),
        raw_event: json!({ "type": "m.room.message", "content": content }),
        megolm_session_id: None,
        redacts: None,
        relates_to: None,
        decrypted_body_text: Some("once"),
    };

    let first = store.upsert_event(&ev).await.expect("first delivery");
    let second = store.upsert_event(&ev).await.expect("re-delivery");
    assert_eq!(
        first, second,
        "a re-delivered event keeps its arrival order (the conflict fallback \
         reads the existing row's id back)"
    );

    // And it is the row's own id — the value `EventDto::arrival_order` carries.
    let page = store
        .room_timeline(account_id, &room_id, None, 10)
        .await
        .expect("timeline");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].id, first, "the reported id is the row's id");

    common::cleanup_account(&pool, account_id).await;
}

/// Arrival order is ingest order, not `origin_ts` order.
///
/// The bridge inversion from ADR 0089: a mautrix portal emits its own state
/// events and *then* backfills the pre-existing conversation with its real,
/// older timestamps. The backfilled message is therefore **last** in arrival
/// order and **first** (oldest) in display order, and a receipt naming the
/// display-newest event does not cover it. This asserts the store keeps the two
/// orders distinct — that `id` follows the sequence and never gets quietly
/// re-derived from `origin_ts` — because every client's receipt selection is
/// built on that being true.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn arrival_order_follows_ingest_not_origin_ts() {
    let store = common::migrated_store().await;
    let pool = common::raw_pool().await;
    let account_id = common::test_account(&store, "arrival-order").await;
    let room_id = format!("!r-{}:localhost", Uuid::new_v4());

    // Timestamps and the inversion from the LinkedIn portal in ADR 0089.
    let create = insert_message(&store, account_id, &room_id, 1_785_928_306_622, "create").await;
    let bridge = insert_message(&store, account_id, &room_id, 1_785_928_309_453, "bridge").await;
    let message = insert_message(&store, account_id, &room_id, 1_785_928_304_987, "message").await;

    let page = store
        .room_timeline(account_id, &room_id, None, 10)
        .await
        .expect("timeline");
    assert_eq!(page.len(), 3);

    let id_of = |wanted: &str| {
        page.iter()
            .find(|row| row.event_id == wanted)
            .expect("event in page")
            .id
    };
    assert!(
        id_of(&create) < id_of(&bridge) && id_of(&bridge) < id_of(&message),
        "ids ascend with ingest even though the last-ingested event is the \
         oldest by origin_ts"
    );

    // Display order (newest-first by origin_ts) disagrees with arrival order:
    // the display-newest event is the bridge one, the arrival-newest is the
    // backfilled message. A receipt has to name the latter.
    assert_eq!(
        page[0].event_id, bridge,
        "display-newest is the bridge event"
    );
    let arrival_newest = page.iter().max_by_key(|row| row.id).expect("non-empty");
    assert_eq!(
        arrival_newest.event_id, message,
        "arrival-newest is the backfilled message"
    );

    common::cleanup_account(&pool, account_id).await;
}
