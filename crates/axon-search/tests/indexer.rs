//! DB-gated end-to-end tests for the indexing actor (ADR 0039): the out-of-order
//! ingestion cases the M10 backfill produces (a relation arriving before its
//! target), and the account-purge path.
//!
//! The actor drains the **global** `search_outbox` and advances a **global**
//! cursor, so — unlike the old per-channel design — two actors must not run at
//! once. These tests therefore serialize on [`INDEXER_SERIAL`]; each opens its own
//! tempdir index but shares the one outbox. They drive the live path, so they spawn
//! with `fresh = false` (no corpus seed) and rely on the post-insert drain.
//!
//! Run with Postgres up:
//!
//! ```sh
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon \
//!   cargo test -p axon-search -- --ignored
//! ```

use std::time::Duration;

use axon_search::{IndexerOptions, SearchIndex, SearchParams};
use axon_store::{NewEvent, Store};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use uuid::Uuid;

const SENDER: &str = "@alice:localhost";

/// Serializes the indexer tests: only one actor may drain the shared outbox /
/// advance the shared cursor at a time. `tokio`'s mutex does not poison on panic,
/// so a failing test still releases it.
static INDEXER_SERIAL: Mutex<()> = Mutex::const_new(());

fn db_url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests")
}

#[allow(clippy::too_many_arguments)]
async fn insert(
    store: &Store,
    account_id: Uuid,
    room: &str,
    event_id: &str,
    ts: i64,
    event_type: &str,
    content: Option<Value>,
    redacts: Option<&str>,
    relates_to: Option<Value>,
) {
    let body = content
        .as_ref()
        .and_then(|c| c.get("body"))
        .and_then(|b| b.as_str())
        .map(str::to_owned);
    store
        .upsert_event(&NewEvent {
            event_id,
            room_id: room,
            account_id,
            sender: SENDER,
            origin_ts: ts,
            event_type,
            content: content.clone(),
            raw_event: json!({ "type": event_type, "content": content }),
            megolm_session_id: None,
            redacts,
            relates_to,
            decrypted_body_text: body.as_deref(),
        })
        .await
        .expect("insert");
}

async fn new_account(store: &Store) -> Uuid {
    store
        .upsert_account(
            &format!("@idx-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .expect("account")
        .account_id
}

/// Spawn an indexer for the **live** path: `fresh = false` skips the corpus seed,
/// with an aggressive cadence for fast, deterministic reads.
fn spawn(store: &Store, index: &SearchIndex) -> axon_search::IndexerHandles {
    index
        .spawn_indexer(
            store.clone(),
            false,
            IndexerOptions {
                channel_capacity: 64,
                batch_size: 100,
                idle_poll_interval: Duration::from_millis(50),
                writer_heap_mb: 15,
                seed_throttle: Duration::ZERO,
            },
        )
        .expect("spawn indexer")
}

/// Poll until `text` (scoped to `account`) returns `expected` hits, or fail.
async fn wait_for_count(index: &SearchIndex, account: Uuid, text: &str, expected: usize) {
    for _ in 0..100 {
        index.reload().expect("reload");
        let res = index
            .search(&SearchParams {
                text,
                account_id: Some(account),
                room_id: None,
                sender: None,
                from_ts: None,
                to_ts: None,
                sort: axon_search::SearchSort::Relevance,
                limit: 10,
                offset: 0,
            })
            .expect("search");
        if res.total == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("query {text:?} never reached {expected} hits for account {account}");
}

fn msg(body: &str) -> Option<Value> {
    Some(json!({ "msgtype": "m.text", "body": body }))
}

/// An edit ingested *before* its target, then the target: the index ends up
/// showing the **edited** body, never the stale original. The fan-out (the edit's
/// outbox row plus its target's) is written by `upsert_event`; the actor only
/// needs a `notify`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Postgres"]
async fn edit_before_target_indexes_edited_body() {
    let _serial = INDEXER_SERIAL.lock().await;
    let store = Store::connect(&db_url(), 5).await.expect("connect");
    let account = new_account(&store).await;
    let room = format!("!room-{}:localhost", Uuid::new_v4());
    let dir = tempfile::tempdir().expect("tempdir");
    let (index, _fresh) = SearchIndex::open(dir.path()).expect("open");
    let handles = spawn(&store, &index);

    let target = format!("$target-{}:localhost", Uuid::new_v4());
    let edit = format!("$edit-{}:localhost", Uuid::new_v4());

    // The EDIT lands first (newest→oldest backfill). Its target doesn't exist yet.
    insert(
        &store,
        account,
        &room,
        &edit,
        200,
        "m.room.message",
        Some(json!({
            "msgtype": "m.text",
            "body": "* anaconda",
            "m.new_content": { "msgtype": "m.text", "body": "anaconda" },
        })),
        None,
        Some(json!({ "rel_type": "m.replace", "event_id": target })),
    )
    .await;
    handles.handle.notify();

    // Then the original target arrives.
    insert(
        &store,
        account,
        &room,
        &target,
        100,
        "m.room.message",
        msg("boa"),
        None,
        None,
    )
    .await;
    handles.handle.notify();

    // The edited body is searchable; the original is not, and the standalone edit
    // is not its own hit.
    wait_for_count(&index, account, "anaconda", 1).await;
    wait_for_count(&index, account, "boa", 0).await;
}

/// A redaction ingested *before* its target, then the target: once the target
/// arrives it is masked, so it is unsearchable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Postgres"]
async fn redaction_before_target_leaves_target_unsearchable() {
    let _serial = INDEXER_SERIAL.lock().await;
    let store = Store::connect(&db_url(), 5).await.expect("connect");
    let account = new_account(&store).await;
    let room = format!("!room-{}:localhost", Uuid::new_v4());
    let dir = tempfile::tempdir().expect("tempdir");
    let (index, _fresh) = SearchIndex::open(dir.path()).expect("open");
    let handles = spawn(&store, &index);

    let target = format!("$target-{}:localhost", Uuid::new_v4());
    let redaction = format!("$redaction-{}:localhost", Uuid::new_v4());

    // The REDACTION lands first.
    insert(
        &store,
        account,
        &room,
        &redaction,
        200,
        "m.room.redaction",
        None,
        Some(&target),
        None,
    )
    .await;
    handles.handle.notify();

    // Then the target message arrives — but it's already redacted.
    insert(
        &store,
        account,
        &room,
        &target,
        100,
        "m.room.message",
        msg("classified"),
        None,
        None,
    )
    .await;
    handles.handle.notify();

    // Never searchable.
    wait_for_count(&index, account, "classified", 0).await;
}

/// A live message is indexed; redacting it later removes it from results.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Postgres"]
async fn redaction_after_index_removes_it() {
    let _serial = INDEXER_SERIAL.lock().await;
    let store = Store::connect(&db_url(), 5).await.expect("connect");
    let account = new_account(&store).await;
    let room = format!("!room-{}:localhost", Uuid::new_v4());
    let dir = tempfile::tempdir().expect("tempdir");
    let (index, _fresh) = SearchIndex::open(dir.path()).expect("open");
    let handles = spawn(&store, &index);

    let target = format!("$target-{}:localhost", Uuid::new_v4());
    insert(
        &store,
        account,
        &room,
        &target,
        100,
        "m.room.message",
        msg("ephemeral"),
        None,
        None,
    )
    .await;
    handles.handle.notify();
    wait_for_count(&index, account, "ephemeral", 1).await;

    // Redact it; the target's doc is re-derived (now masked) and removed.
    let redaction = format!("$redaction-{}:localhost", Uuid::new_v4());
    insert(
        &store,
        account,
        &room,
        &redaction,
        200,
        "m.room.redaction",
        None,
        Some(&target),
        None,
    )
    .await;
    handles.handle.notify();
    wait_for_count(&index, account, "ephemeral", 0).await;
}

/// Deleting the account row appends the purge obligation transactionally, and a
/// `flush` drains it so every document for the account is gone — the synchronous
/// path the lifecycle verb uses when the indexer is live.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Postgres"]
async fn delete_account_purges_documents() {
    let _serial = INDEXER_SERIAL.lock().await;
    let store = Store::connect(&db_url(), 5).await.expect("connect");
    let account = new_account(&store).await;
    let room = format!("!room-{}:localhost", Uuid::new_v4());
    let dir = tempfile::tempdir().expect("tempdir");
    let (index, _fresh) = SearchIndex::open(dir.path()).expect("open");
    let handles = spawn(&store, &index);

    for i in 0..3 {
        let id = format!("$m{i}-{}:localhost", Uuid::new_v4());
        insert(
            &store,
            account,
            &room,
            &id,
            100 + i,
            "m.room.message",
            msg("purgeable note"),
            None,
            None,
        )
        .await;
    }
    handles.handle.notify();
    wait_for_count(&index, account, "purgeable", 3).await;

    // The store deletes the row and appends the purge sentinel atomically; the
    // flush drains it before returning.
    store
        .delete_account_row(account)
        .await
        .expect("delete account row");
    handles.handle.flush().await;
    wait_for_count(&index, account, "purgeable", 0).await;
}

/// The indexing actor's seed-completion marker file, written inside the index
/// directory only after the corpus seed succeeds (kept in lockstep with
/// `index::SCHEMA_VERSION_FILE`, which is crate-private).
const SEED_MARKER_FILE: &str = "axon_schema_version";

/// A fresh index seeds the whole corpus from the event store (the rebuild source),
/// making an account's pre-existing messages searchable with no live `notify` —
/// the path a new/empty/schema-bumped directory takes on boot. The actor stamps the
/// seed-completion marker only after the seed finishes, so a later reopen trusts the
/// index instead of reseeding.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Postgres"]
async fn fresh_index_seeds_from_corpus() {
    let _serial = INDEXER_SERIAL.lock().await;
    let store = Store::connect(&db_url(), 5).await.expect("connect");
    let account = new_account(&store).await;
    let room = format!("!room-{}:localhost", Uuid::new_v4());

    // Persist a message *before* any indexer exists (it lands only in Postgres).
    let id = format!("$seed-{}:localhost", Uuid::new_v4());
    insert(
        &store,
        account,
        &room,
        &id,
        100,
        "m.room.message",
        msg("seedling"),
        None,
        None,
    )
    .await;

    // A fresh directory ⇒ seed from the corpus; no notify is sent.
    let dir = tempfile::tempdir().expect("tempdir");
    let (index, fresh) = SearchIndex::open(dir.path()).expect("open");
    assert!(fresh);
    let _handles = index
        .spawn_indexer(
            store.clone(),
            true,
            IndexerOptions {
                channel_capacity: 64,
                batch_size: 100,
                idle_poll_interval: Duration::from_millis(50),
                writer_heap_mb: 15,
                seed_throttle: Duration::ZERO,
            },
        )
        .expect("spawn indexer");

    wait_for_count(&index, account, "seedling", 1).await;

    // The seed-completion marker lands only after the seed finishes (just after the
    // doc is searchable), so poll for it. Its presence is what makes a later boot
    // trust this index; an interrupted seed would leave it absent and reseed.
    let marker = dir.path().join(SEED_MARKER_FILE);
    for _ in 0..100 {
        if marker.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        marker.exists(),
        "the actor stamps the seed-completion marker after seeding"
    );
    // A reopen now sees the marker and reuses the index instead of wiping it.
    let (_reopened, fresh_again) = SearchIndex::open(dir.path()).expect("reopen");
    assert!(
        !fresh_again,
        "a marked, fully-seeded index reopens in place"
    );
}
