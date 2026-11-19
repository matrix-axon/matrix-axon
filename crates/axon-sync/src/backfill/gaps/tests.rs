//! The ADR 0140 delivery step 1 unit tests: real sliding-sync responses from a
//! stand-in homeserver, stored by axon's live ingestion handler, read by the
//! real watcher, and filled by the real filler from the stand-in's
//! `/messages`. Each test drives the watcher by hand, so it controls when the
//! watcher reads a response relative to that response's handlers.
//!
//! Require Postgres (`DATABASE_URL`) and are `#[ignore]`d by default.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Mutex;

use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::sliding_sync::SlidingSync;
use matrix_sdk::store::RoomLoadSettings;
use matrix_sdk::{SessionMeta, SessionTokens};
use serde_json::{json, Value};

use super::*;
use crate::backfill::{backfill_room_page, PageOutcome};
use crate::engine::persist_timeline_event;

/// Small pages, so every hole takes several and the stop placement shows in
/// the request log.
pub(crate) const PAGE: u32 = 4;

/// The in-handler insert budget for harnesses with a recorder: long enough
/// for a few attempts, short enough that a refused hole reaches memory fast.
const INSERT_BUDGET_FOR_TESTS: Duration = Duration::from_millis(500);

/// The stand-in homeserver's state: each room's timeline in stream order, the
/// queued sliding-sync responses, rejected tokens, and every `/messages`
/// request's `(room, from)`.
#[derive(Default)]
pub(crate) struct Upstream {
    pub(crate) rooms: HashMap<String, Vec<Value>>,
    pub(crate) sync: VecDeque<Value>,
    pub(crate) rejected: HashSet<String>,
    pub(crate) messages: Vec<(String, Option<String>)>,
}

pub(crate) type Shared = Arc<Mutex<Upstream>>;

/// Token `s{n}` means "before stream position `n`"; no token is the live end.
fn position(token: Option<&str>, len: usize) -> usize {
    token
        .and_then(|t| t.strip_prefix('s'))
        .and_then(|n| n.parse().ok())
        .unwrap_or(len)
        .min(len)
}

async fn messages(
    State(upstream): State<Shared>,
    Path(room_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> axum::response::Response {
    let from = query.get("from").cloned();
    let limit = query
        .get("limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(10);
    let mut upstream = upstream.lock().unwrap();
    upstream.messages.push((room_id.clone(), from.clone()));
    if from
        .as_ref()
        .is_some_and(|from| upstream.rejected.contains(from))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"errcode": "M_UNKNOWN", "error": "Invalid stream token"})),
        )
            .into_response();
    }
    let events = upstream.rooms.get(&room_id).cloned().unwrap_or_default();
    let high = position(from.as_deref(), events.len());
    let low = high.saturating_sub(limit);
    let chunk: Vec<Value> = events[low..high].iter().rev().cloned().collect();
    let mut body = json!({"chunk": chunk, "start": format!("s{high}")});
    if low > 0 {
        body["end"] = json!(format!("s{low}"));
    }
    Json(body).into_response()
}

/// What one sync response delivers for one room: its upstream events
/// `[from, to)`.
pub(crate) struct Delivery {
    tag: &'static str,
    from: usize,
    to: usize,
    limited: bool,
}

pub(crate) fn deliver(tag: &'static str, from: usize, to: usize) -> Delivery {
    Delivery {
        tag,
        from,
        to,
        limited: false,
    }
}

impl Delivery {
    pub(crate) fn limited(mut self) -> Self {
        self.limited = true;
        self
    }
}

pub(crate) struct Harness {
    pub(crate) store: Store,
    pub(crate) account_id: Uuid,
    pub(crate) user: String,
    pub(crate) client: Client,
    pub(crate) sync: SlidingSync,
    pub(crate) upstream: Shared,
    pub(crate) updates: broadcast::Receiver<RoomUpdates>,
    pub(crate) ctx: PersistContext,
    pub(crate) signal: Arc<GapSignal>,
    pub(crate) gate: Arc<HomeserverGate>,
    pub(crate) health: BackfillHealth,
    pub(crate) cancel: CancellationToken,
    pub(crate) rooms: HashMap<&'static str, String>,
    pub(crate) pos: u64,
    pub(crate) server: tokio::task::JoinHandle<()>,
    /// The gap recorder the ingestion handler drains, as in the engine; `None`
    /// for tests that hand updates to a watcher themselves.
    pub(crate) recorder: Option<Arc<GapRecorder>>,
}

impl Harness {
    /// A harness whose tests drive a [`GapWatcher`] by hand.
    pub(crate) async fn new(tags: &[&'static str]) -> Self {
        Self::build(tags, false).await
    }

    /// A harness whose ingestion handler drains a [`GapRecorder`], as the
    /// engine's does.
    pub(crate) async fn with_recorder(tags: &[&'static str]) -> Self {
        Self::build(tags, true).await
    }

    pub(crate) async fn build(tags: &[&'static str], inline: bool) -> Self {
        let store = Store::connect(&std::env::var("DATABASE_URL").unwrap(), 5)
            .await
            .unwrap();
        let user = format!("@gaps-{}:localhost", Uuid::new_v4());
        let account_id = store
            .upsert_account(&user, "http://localhost")
            .await
            .unwrap()
            .account_id;
        let rooms: HashMap<&'static str, String> = tags
            .iter()
            .map(|tag| (*tag, format!("!{tag}-{}:localhost", Uuid::new_v4())))
            .collect();
        let upstream = Shared::default();
        {
            let mut up = upstream.lock().unwrap();
            for room_id in rooms.values() {
                up.rooms.insert(room_id.clone(), Vec::new());
            }
        }
        let sync_queue = Arc::clone(&upstream);
        let app = Router::new()
            .route(
                "/_matrix/client/v3/rooms/{room_id}/messages",
                get(messages).with_state(Arc::clone(&upstream)),
            )
            .fallback(move |request: Request| {
                let queue = Arc::clone(&sync_queue);
                async move {
                    if request.method() == axum::http::Method::POST
                        && request.uri().path().ends_with("/sync")
                    {
                        let next = queue.lock().unwrap().sync.pop_front();
                        Json(next.expect("a queued sync response")).into_response()
                    } else {
                        (
                            StatusCode::NOT_FOUND,
                            Json(json!({"errcode": "M_NOT_FOUND", "error": "fixture"})),
                        )
                            .into_response()
                    }
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address: SocketAddr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = Client::builder()
            .homeserver_url(format!("http://{address}"))
            .server_versions([matrix_sdk::ruma::api::MatrixVersion::V1_11])
            .request_config(matrix_sdk::config::RequestConfig::new().disable_retry())
            .build()
            .await
            .unwrap();
        client
            .matrix_auth()
            .restore_session(
                MatrixSession {
                    meta: SessionMeta {
                        user_id: user.parse().unwrap(),
                        device_id: "GAPTEST".into(),
                    },
                    tokens: SessionTokens {
                        access_token: "fixture-token".to_owned(),
                        refresh_token: None,
                    },
                },
                RoomLoadSettings::default(),
            )
            .await
            .unwrap();
        let (live_tx, _) = broadcast::channel(64);
        let (state_redaction_tx, _) = tokio::sync::mpsc::channel(32);
        let signal = Arc::new(GapSignal::default());
        let recorder = if inline {
            let watcher = GapWatcher::new(account_id, store.clone(), 5, Arc::clone(&signal))
                .await
                .with_insert_budget(INSERT_BUDGET_FOR_TESTS);
            Some(Arc::new(GapRecorder::new(
                client.subscribe_to_all_room_updates(),
                watcher,
            )))
        } else {
            None
        };
        let ctx = PersistContext {
            store: store.clone(),
            account_id,
            live_tx: live_tx.clone(),
            index: None,
            local_user_id: Arc::from(user.as_str()),
            purge_on_leave: false,
            state_redaction_tx,
            room_details: crate::room_details::RoomDetailsChanges::new(account_id, live_tx),
            gaps: recorder.clone(),
        };
        client.add_event_handler_context(ctx.clone());
        client.add_event_handler(persist_timeline_event);
        // Subscribed before the first response, as the engine does.
        let updates = client.subscribe_to_all_room_updates();
        let sync = client
            .sliding_sync("gap-test")
            .unwrap()
            .build()
            .await
            .unwrap();
        let ids: Vec<matrix_sdk::ruma::OwnedRoomId> =
            rooms.values().map(|id| id.parse().unwrap()).collect();
        let refs: Vec<&matrix_sdk::ruma::RoomId> = ids.iter().map(AsRef::as_ref).collect();
        sync.add_room_subscriptions(&refs, None, false);
        Self {
            store,
            account_id,
            user,
            client,
            sync,
            upstream,
            updates,
            ctx,
            signal,
            gate: HomeserverGate::for_test(),
            health: BackfillHealth::new(None),
            cancel: CancellationToken::new(),
            rooms,
            pos: 0,
            server,
            recorder,
        }
    }

    pub(crate) fn room(&self, tag: &str) -> &str {
        &self.rooms[tag]
    }

    pub(crate) fn event_id(tag: &str, i: usize) -> String {
        format!("${tag}-{i}:localhost")
    }

    /// Add `n` events to a room upstream.
    pub(crate) fn append(&self, tag: &str, n: usize) {
        let room_id = self.room(tag).to_owned();
        let mut up = self.upstream.lock().unwrap();
        let events = up.rooms.get_mut(&room_id).unwrap();
        let start = events.len();
        for i in start..start + n {
            events.push(json!({
                "type": "m.room.message",
                "sender": self.user,
                "event_id": Self::event_id(tag, i),
                "origin_server_ts": 1_700_000_000_000_u64 + i as u64,
                "content": {"msgtype": "m.text", "body": format!("{tag} {i}")},
            }));
        }
    }

    /// Serve one sliding-sync response and let the SDK process it, so axon's
    /// handlers have stored its events. Returns its room-update broadcast,
    /// which the test hands to the watcher whenever it chooses.
    pub(crate) async fn sync(&mut self, deliveries: &[Delivery]) -> RoomUpdates {
        self.queue(deliveries);
        tokio::time::timeout(Duration::from_secs(10), self.sync.sync_once())
            .await
            .unwrap()
            .unwrap();
        // Each insert is its own transaction; keep storage times distinct
        // between responses on a coarse database clock.
        tokio::time::sleep(Duration::from_millis(5)).await;
        self.updates.try_recv().expect("the response's broadcast")
    }

    /// Queue the next sliding-sync response without serving it.
    pub(crate) fn queue(&mut self, deliveries: &[Delivery]) {
        self.pos += 1;
        let mut rooms = serde_json::Map::new();
        {
            let up = self.upstream.lock().unwrap();
            for d in deliveries {
                let room_id = self.room(d.tag);
                let timeline: Vec<Value> = up.rooms[room_id][d.from..d.to].to_vec();
                rooms.insert(
                    room_id.to_owned(),
                    json!({
                        "initial": self.pos == 1,
                        "required_state": [{
                            "type": "m.room.member", "state_key": self.user,
                            "sender": self.user, "event_id": format!("$join-{}", d.tag),
                            "origin_server_ts": 1, "content": {"membership": "join"},
                        }],
                        "timeline": timeline,
                        "limited": d.limited,
                        "prev_batch": format!("s{}", d.from),
                    }),
                );
            }
        }
        self.upstream
            .lock()
            .unwrap()
            .sync
            .push_back(json!({"pos": self.pos.to_string(), "lists": {}, "rooms": rooms}));
    }

    pub(crate) async fn watcher(&self) -> GapWatcher {
        GapWatcher::new(
            self.account_id,
            self.store.clone(),
            5,
            Arc::clone(&self.signal),
        )
        .await
    }

    /// Every room of the harness, as the watcher reads the joined rooms.
    pub(crate) fn joined_rooms(&self) -> impl Fn() -> Vec<String> + Send + Sync {
        let rooms: Vec<String> = self.rooms.values().cloned().collect();
        move || rooms.clone()
    }

    pub(crate) fn params(&self, enabled: bool) -> BackfillParams {
        BackfillParams {
            enabled,
            verify_enabled: false,
            timeline_limit: 5,
            page_size: PAGE,
            target_depth: 0,
            throttle: Duration::ZERO,
            page_timeout: Duration::from_secs(5),
            idle_poll: Duration::from_secs(1),
            min_free_bytes: 0,
            min_free_percent: 0.0,
            guard_path: std::env::temp_dir(),
        }
    }

    pub(crate) async fn fill(&self) -> GapPass {
        let params = self.params(false);
        Filler {
            client: &self.client,
            ctx: &self.ctx,
            params: &params,
            health: &self.health,
            gate: &self.gate,
            cancel: &self.cancel,
        }
        .fill()
        .await
    }

    /// One ordinary backfill page, as the backfill task would page it.
    pub(crate) async fn backfill_page(&self, tag: &str) -> PageOutcome {
        let room = self.joined(tag);
        let state = self
            .store
            .get_room_backfill(self.account_id, self.room(tag))
            .await
            .unwrap();
        backfill_room_page(
            &self.ctx,
            &self.params(true),
            &self.gate,
            &room,
            state,
            &self.cancel,
        )
        .await
    }

    pub(crate) fn joined(&self, tag: &str) -> matrix_sdk::Room {
        let room = self
            .client
            .get_room(&matrix_sdk::ruma::RoomId::parse(self.room(tag)).unwrap())
            .expect("room known to the SDK");
        assert_eq!(room.state(), RoomState::Joined);
        room
    }

    /// The upstream positions of a room's stored events.
    pub(crate) async fn stored(&self, tag: &str) -> BTreeSet<usize> {
        let ids: Vec<(String,)> = sqlx_core::query_as::query_as(
            "SELECT event_id FROM events WHERE account_id = $1 AND room_id = $2",
        )
        .bind(self.account_id)
        .bind(self.room(tag))
        .fetch_all(self.store.pool())
        .await
        .unwrap();
        let prefix = format!("${tag}-");
        ids.into_iter()
            .filter_map(|(id,)| {
                id.strip_prefix(&prefix)?
                    .strip_suffix(":localhost")?
                    .parse()
                    .ok()
            })
            .collect()
    }

    pub(crate) async fn gaps(&self) -> Vec<TimelineGap> {
        self.store
            .list_timeline_gaps(self.account_id)
            .await
            .unwrap()
    }

    /// The `from` of every `/messages` request for a room since the last call.
    pub(crate) fn take_requests(&self, tag: &str) -> Vec<Option<String>> {
        let room_id = self.room(tag).to_owned();
        let mut up = self.upstream.lock().unwrap();
        let (mine, rest): (Vec<_>, Vec<_>) = up
            .messages
            .drain(..)
            .partition(|(room, _)| *room == room_id);
        up.messages = rest;
        mine.into_iter().map(|(_, from)| from).collect()
    }

    pub(crate) async fn finish(self) {
        self.store
            .delete_account_row(self.account_id)
            .await
            .unwrap();
        self.server.abort();
    }
}

pub(crate) fn range(from: usize, to: usize) -> BTreeSet<usize> {
    (from..to).collect()
}

pub(crate) fn tokens(positions: &[usize]) -> Vec<Option<String>> {
    positions.iter().map(|p| Some(format!("s{p}"))).collect()
}

/// Two holes stacked in one room are each filled from their own row, and each
/// walk stops at the history stored before its hole: the walk for the newer
/// hole at the older hole's limited batch, never at the room's start. A third
/// hole that another walk already filled is still walked to its own boundary,
/// because events stored after it was detected never stop it.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn nested_holes_are_filled_to_their_own_boundaries() {
    let mut h = Harness::new(&["a"]).await;
    let mut watcher = h.watcher().await;
    h.append("a", 10);
    // The room's first window, then two limited responses: holes 10..25 and
    // 30..45.
    let u0 = h.sync(&[deliver("a", 5, 10).limited()]).await;
    watcher.observe(&u0).await;
    h.append("a", 20);
    let u1 = h.sync(&[deliver("a", 25, 30).limited()]).await;
    watcher.observe(&u1).await;
    h.append("a", 20);
    let u2 = h.sync(&[deliver("a", 45, 50).limited()]).await;
    watcher.observe(&u2).await;
    assert_eq!(h.gaps().await.len(), 3);
    assert_eq!(
        h.stored("a").await,
        &(&range(5, 10) | &range(25, 30)) | &range(45, 50)
    );

    let pass = h.fill().await;
    assert!(pass.progressed);
    assert!(h.gaps().await.is_empty(), "every row closed or dropped");
    assert_eq!(h.stored("a").await, range(5, 50), "both holes filled");
    // Newest row first in each round; a row closes on the page that meets the
    // previous response's batch (s29 and s9 hold events 29 and 9). The first
    // window is dropped unpaged, so nothing before position 5 is requested.
    let mut requests = h.take_requests("a");
    requests.sort();
    let mut expected = tokens(&[45, 41, 37, 33, 25, 21, 17, 13]);
    expected.sort();
    assert_eq!(requests, expected);

    // A lag hides one plain response (50..52); the next one, limited, is
    // read after the lag: hole 52..65 gets its own row, after a probe that
    // covers the same history.
    h.append("a", 2);
    h.sync(&[deliver("a", 50, 52)]).await;
    h.append("a", 18);
    let u4 = h.sync(&[deliver("a", 65, 70).limited()]).await;
    watcher.lagged(1, &h.joined_rooms()).await;
    watcher.observe(&u4).await;
    let rows = h.gaps().await;
    assert_eq!(rows.len(), 2);
    let (hole, probe) = (&rows[0], &rows[1]);
    assert_eq!(
        (hole.origin, probe.origin),
        (GapOrigin::Limited, GapOrigin::LagProbe)
    );
    // Hold the hole's row back, so the probe fills its history first.
    sqlx_core::query::query(
        "UPDATE room_timeline_gaps SET retry_at = now() + interval '1 hour' WHERE id = $1",
    )
    .bind(hole.id)
    .execute(h.store.pool())
    .await
    .unwrap();
    h.fill().await;
    assert_eq!(
        h.stored("a").await,
        range(5, 70),
        "the probe filled the hole"
    );
    assert_eq!(
        h.take_requests("a"),
        [vec![None], tokens(&[66, 62, 58, 54, 50])].concat(),
        "the probe walks back to the last update the watcher read (45..50)"
    );
    // The hole's own row walks through what the probe stored, because that
    // was stored after the hole was detected, and stops at 49, stored before.
    sqlx_core::query::query("UPDATE room_timeline_gaps SET retry_at = NULL WHERE id = $1")
        .bind(hole.id)
        .execute(h.store.pool())
        .await
        .unwrap();
    h.fill().await;
    assert!(h.gaps().await.is_empty());
    assert_eq!(h.take_requests("a"), tokens(&[65, 61, 57, 53]));
    h.finish().await;
}

/// A rejected token restarts the row from the live end, bounded by the
/// batch's earliest storage time: the walk passes through the batch and the
/// live events after it, fills the hole, and stops at the history before it.
/// A crash after the restarted row's first page resumes with the same rule.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_rejected_token_restarts_from_the_live_end_and_survives_a_crash() {
    let mut h = Harness::new(&["r"]).await;
    let mut watcher = h.watcher().await;
    h.append("r", 5);
    let u0 = h.sync(&[deliver("r", 0, 5)]).await;
    watcher.observe(&u0).await;
    h.append("r", 30);
    let u1 = h.sync(&[deliver("r", 30, 35).limited()]).await;
    watcher.observe(&u1).await;
    h.append("r", 5);
    let u2 = h.sync(&[deliver("r", 35, 40)]).await;
    watcher.observe(&u2).await;
    h.upstream.lock().unwrap().rejected.insert("s30".to_owned());

    // The first page is rejected; the row restarts from the live end.
    h.fill().await;
    assert_eq!(h.take_requests("r"), tokens(&[30]));
    let row = h.gaps().await.pop().expect("row kept");
    assert_eq!(row.origin, GapOrigin::TokenRejected);
    assert_eq!(row.from_token, None);
    let batch_min = h
        .store
        .lag_mark(h.account_id, &[Harness::event_id("r", 30)])
        .await
        .unwrap()
        .map(|mark| mark.received_at);
    assert_eq!(row.stored_before, batch_min);

    // One page from the live end, then a crash: the filler goes away with the
    // row part way, and only what the store holds carries over.
    let params = h.params(false);
    let one_page = Filler {
        client: &h.client,
        ctx: &h.ctx,
        params: &params,
        health: &h.health,
        gate: &h.gate,
        cancel: &h.cancel,
    }
    .page(&h.joined("r"), &row)
    .await;
    assert!(matches!(one_page, GapPage::Paged));
    let row = h.gaps().await.pop().expect("row kept");
    assert_eq!(row.from_token.as_deref(), Some("s36"));
    assert_eq!(row.stored_before, batch_min, "the boundary survives");

    // A fresh filler resumes and stops at position 4, stored before the hole.
    h.fill().await;
    assert!(h.gaps().await.is_empty());
    assert_eq!(h.stored("r").await, range(0, 40));
    assert_eq!(
        h.take_requests("r"),
        [vec![None], tokens(&[36, 32, 28, 24, 20, 16, 12, 8])].concat()
    );
    h.finish().await;
}

/// A lag hides two limited responses in one room and a room's first
/// appearance. The watcher stalled after its last read, longer than the lag:
/// the probe boundary still comes from what that read stored, so both holes
/// are filled, and the new room's probe closes at its first window instead of
/// walking to its start.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_lag_probe_fills_every_hole_in_the_window() {
    let mut h = Harness::new(&["a", "b"]).await;
    let mut watcher = h.watcher().await;
    h.append("a", 10);
    let u0 = h.sync(&[deliver("a", 5, 10).limited()]).await;
    watcher.observe(&u0).await;

    // The watcher stalls from here: everything below is stored by the
    // handlers but lost from the broadcast.
    h.append("a", 3);
    h.sync(&[deliver("a", 10, 13)]).await;
    h.append("a", 22);
    h.sync(&[deliver("a", 30, 35).limited()]).await;
    h.append("a", 20);
    h.sync(&[deliver("a", 50, 55).limited()]).await;
    h.append("b", 30);
    h.sync(&[deliver("b", 25, 30).limited()]).await;
    assert_eq!(
        h.stored("a").await,
        &(&range(5, 13) | &range(30, 35)) | &range(50, 55)
    );
    // The watcher stays stalled for longer than the lag took, then notices
    // it. A boundary taken from the clock when it notices, with any margin
    // shorter than the stall, would lie after the lost responses and miss
    // both holes.
    tokio::time::sleep(Duration::from_millis(500)).await;
    watcher.lagged(4, &h.joined_rooms()).await;

    h.fill().await;
    assert!(h.gaps().await.is_empty());
    // Both holes (13..30 and 35..50) filled; nothing before the first window.
    assert_eq!(h.stored("a").await, range(5, 55));
    // The probe stops at the page holding 7..10, where 7 and 8 were stored
    // before the boundary (the latest of 5..9), and never asks for anything
    // older.
    assert_eq!(
        h.take_requests("a"),
        [
            vec![None],
            tokens(&[51, 47, 43, 39, 35, 31, 27, 23, 19, 15, 11])
        ]
        .concat()
    );
    // The room first seen during the lag: its probe closes on the page that
    // holds the first event stored there (25, the oldest of its first
    // window), and that page is written in full. Its earlier history stays
    // backfill's job.
    assert_eq!(h.stored("b").await, range(22, 30));
    assert_eq!(h.take_requests("b"), [vec![None], tokens(&[26])].concat());
    h.finish().await;
}

/// The first-window test holds whenever the watcher reads the broadcast:
/// after this response's handlers stored its batch, or after a later
/// response's handlers stored more, and whether or not backfill stored older
/// history before the row was checked. None of these is paged.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_first_window_is_never_paged_however_late_the_watcher_reads_it() {
    for later_response in [false, true] {
        for backfill in [false, true] {
            let case = format!("later_response={later_response} backfill={backfill}");
            let mut h = Harness::new(&["c"]).await;
            let mut watcher = h.watcher().await;
            h.append("c", 30);
            let first = h.sync(&[deliver("c", 25, 30).limited()]).await;
            let mut expected = range(25, 30);
            if later_response {
                h.append("c", 3);
                let next = h.sync(&[deliver("c", 30, 33)]).await;
                // Read only now, after both responses' handlers ran.
                watcher.observe(&first).await;
                watcher.observe(&next).await;
                expected = range(25, 33);
            } else {
                watcher.observe(&first).await;
            }
            assert_eq!(h.gaps().await.len(), 1, "{case}: row recorded");
            if backfill {
                // Ordinary backfill stores older history before the row is
                // checked; it was stored after the batch, so it changes
                // nothing.
                for _ in 0..10 {
                    if h.stored("c").await.contains(&20) {
                        break;
                    }
                    h.backfill_page("c").await;
                }
                assert!(h.stored("c").await.contains(&20), "{case}: backfill ran");
                expected = &expected | &range(h.stored("c").await.first().copied().unwrap(), 25);
            }
            h.take_requests("c");

            let pass = h.fill().await;
            assert!(!pass.progressed, "{case}");
            assert!(h.gaps().await.is_empty(), "{case}: dropped unpaged");
            assert!(h.take_requests("c").is_empty(), "{case}: no gap page");
            assert_eq!(h.stored("c").await, expected, "{case}");
            h.finish().await;
        }
    }
}

/// Makes `table` refuse the first `times` inserts of rows matching `when`, as a
/// failing write would. The count lives in a sequence, so a refused insert's
/// rollback does not undo it. `when` names a room id no other test uses.
pub(crate) struct InjectedFailures {
    store: Store,
    name: String,
    table: &'static str,
}

/// A deadlock: a refusal that is not about the event, so the page is held.
pub(crate) const TRANSIENT: &str = "40P01";

/// What Postgres raises for a `\u0000` escape in a `jsonb`: a rejection of
/// the event's values, which every attempt meets again, so it is skipped.
pub(crate) const PERMANENT: &str = "22P05";

impl InjectedFailures {
    /// Refuse inserts with an error that holds the page ([`TRANSIENT`]).
    pub(crate) async fn install(
        store: &Store,
        table: &'static str,
        when: &str,
        times: u32,
    ) -> Self {
        Self::install_with(store, table, "INSERT", TRANSIENT, when, times).await
    }

    /// Refuse `op` (`INSERT` or `UPDATE`) statements with SQLSTATE `errcode`.
    pub(crate) async fn install_with(
        store: &Store,
        table: &'static str,
        op: &str,
        errcode: &str,
        when: &str,
        times: u32,
    ) -> Self {
        let name = format!("gap_test_{}", Uuid::new_v4().simple());
        for sql in [
            format!("CREATE SEQUENCE {name}"),
            format!(
                "CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ \
                 BEGIN \
                     IF nextval('{name}') <= {times} THEN \
                         RAISE EXCEPTION 'injected failure' USING ERRCODE = '{errcode}'; \
                     END IF; \
                     RETURN NEW; \
                 END $$"
            ),
            format!(
                "CREATE TRIGGER {name} BEFORE {op} ON {table} FOR EACH ROW \
                 WHEN ({when}) EXECUTE FUNCTION {name}()"
            ),
        ] {
            sqlx_core::query::query(&sql)
                .execute(store.pool())
                .await
                .unwrap();
        }
        Self {
            store: store.clone(),
            name,
            table,
        }
    }

    /// How many inserts reached the trigger.
    pub(crate) async fn attempts(&self) -> i64 {
        let (attempts,): (i64,) = sqlx_core::query_as::query_as(&format!(
            "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM {}",
            self.name
        ))
        .fetch_one(self.store.pool())
        .await
        .unwrap();
        attempts
    }

    pub(crate) async fn remove(self) {
        let name = &self.name;
        for sql in [
            format!("DROP TRIGGER {name} ON {}", self.table),
            format!("DROP FUNCTION {name}()"),
            format!("DROP SEQUENCE {name}"),
        ] {
            sqlx_core::query::query(&sql)
                .execute(self.store.pool())
                .await
                .unwrap();
        }
    }
}

/// Review finding: a probe's first-window stop must precede the hole even
/// when the room's first window was shorter than the timeline limit, whether
/// that window was read before the lag or lost in it. Several lowest-id
/// events would include the later limited batch, and the probe would close on
/// its first page with the hole untouched.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_short_first_window_does_not_stop_a_lag_probe_inside_the_hole() {
    let mut h = Harness::new(&["s", "d"]).await;
    let mut watcher = h.watcher().await;
    // "s": a one-event first window, read before the lag.
    h.append("s", 1);
    let u0 = h.sync(&[deliver("s", 0, 1)]).await;
    watcher.observe(&u0).await;
    // "d": a one-event first window, lost in the lag.
    h.append("d", 1);
    h.sync(&[deliver("d", 0, 1)]).await;
    // Both rooms then get a limited response the watcher also loses.
    h.append("s", 24);
    h.append("d", 24);
    h.sync(&[
        deliver("s", 20, 25).limited(),
        deliver("d", 20, 25).limited(),
    ])
    .await;
    watcher.lagged(2, &h.joined_rooms()).await;

    h.fill().await;
    assert!(h.gaps().await.is_empty());
    for tag in ["s", "d"] {
        assert_eq!(h.stored(tag).await, range(0, 25), "{tag}: hole filled");
        assert_eq!(
            h.take_requests(tag),
            [vec![None], tokens(&[21, 17, 13, 9, 5, 1])].concat(),
            "{tag}: walked down to the room's first stored event"
        );
    }
    h.finish().await;
}

/// Review finding: an event the store refuses must not be skipped. The row
/// keeps its token and backs off, and the next attempt stores the event and
/// only then closes.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_failed_event_write_keeps_the_row_until_its_page_is_stored() {
    let mut h = Harness::new(&["w"]).await;
    let mut watcher = h.watcher().await;
    h.append("w", 5);
    let u0 = h.sync(&[deliver("w", 0, 5)]).await;
    watcher.observe(&u0).await;
    h.append("w", 30);
    let u1 = h.sync(&[deliver("w", 30, 35).limited()]).await;
    watcher.observe(&u1).await;
    // Fail one write on the page that reaches the history before the hole:
    // the page from s6 holds 2..5, where 2..4 were stored before it, so
    // without the failure it would close the row.
    let failures = InjectedFailures::install(
        &h.store,
        "events",
        &format!(
            "NEW.room_id = '{}' AND NEW.event_id = '{}'",
            h.room("w"),
            Harness::event_id("w", 5)
        ),
        1,
    )
    .await;

    h.fill().await;
    assert_eq!(failures.attempts().await, 1, "the write was refused once");
    let row = h.gaps().await.pop().expect("the row stays open");
    assert_eq!(row.from_token.as_deref(), Some("s6"), "its token is kept");
    assert_eq!(row.consecutive_failures, 1, "and it backs off");
    assert_eq!(h.stored("w").await, &range(0, 5) | &range(6, 35));

    // Once the backoff ends, the page is fetched again, 5 is stored, and the
    // row closes.
    sqlx_core::query::query("UPDATE room_timeline_gaps SET retry_at = NULL WHERE id = $1")
        .bind(row.id)
        .execute(h.store.pool())
        .await
        .unwrap();
    h.take_requests("w");
    h.fill().await;
    assert!(h.gaps().await.is_empty());
    assert_eq!(h.stored("w").await, range(0, 35));
    assert_eq!(h.take_requests("w"), tokens(&[6]));
    failures.remove().await;
    h.finish().await;
}

/// Review finding (#1143): a gap page with an event the store can never
/// accept still closes the row; only that event is missing.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_gap_page_skips_an_event_the_store_can_never_accept() {
    let mut h = Harness::new(&["v"]).await;
    let mut watcher = h.watcher().await;
    h.append("v", 5);
    let u0 = h.sync(&[deliver("v", 0, 5)]).await;
    watcher.observe(&u0).await;
    h.append("v", 30);
    let u1 = h.sync(&[deliver("v", 30, 35).limited()]).await;
    watcher.observe(&u1).await;
    let failures = InjectedFailures::install_with(
        &h.store,
        "events",
        "INSERT",
        PERMANENT,
        &format!(
            "NEW.room_id = '{}' AND NEW.event_id = '{}'",
            h.room("v"),
            Harness::event_id("v", 20)
        ),
        u32::MAX,
    )
    .await;

    h.fill().await;
    assert!(h.gaps().await.is_empty(), "closed");
    assert_eq!(failures.attempts().await, 1, "tried once");
    assert_eq!(h.stored("v").await, &range(0, 20) | &range(21, 35));
    failures.remove().await;
    h.finish().await;
}

/// Review finding (#1143): a store error saving a row's progress backs the
/// row off, as a homeserver failure does, rather than leaving it due so the
/// next sweep fetches the same page again.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_failed_progress_save_backs_the_row_off() {
    let mut h = Harness::new(&["p"]).await;
    let mut watcher = h.watcher().await;
    h.append("p", 5);
    let u0 = h.sync(&[deliver("p", 0, 5)]).await;
    watcher.observe(&u0).await;
    h.append("p", 30);
    let u1 = h.sync(&[deliver("p", 30, 35).limited()]).await;
    watcher.observe(&u1).await;
    // Refuse the update that moves the row's token, once. The first-window
    // check and the failure record leave the token alone.
    let failures = InjectedFailures::install_with(
        &h.store,
        "room_timeline_gaps",
        "UPDATE",
        TRANSIENT,
        &format!(
            "NEW.room_id = '{}' AND NEW.from_token IS DISTINCT FROM OLD.from_token",
            h.room("p")
        ),
        1,
    )
    .await;

    h.take_requests("p");
    h.fill().await;
    assert_eq!(failures.attempts().await, 1, "the save was refused");
    assert_eq!(h.take_requests("p"), tokens(&[30]));
    let row = h.gaps().await.pop().expect("the row stays open");
    assert_eq!(row.from_token.as_deref(), Some("s30"), "its token is kept");
    assert_eq!(row.consecutive_failures, 1, "and it backs off");
    assert!(!row.retry_due);
    // The next sweep leaves it alone.
    h.fill().await;
    assert!(h.take_requests("p").is_empty(), "not fetched again yet");

    sqlx_core::query::query("UPDATE room_timeline_gaps SET retry_at = NULL WHERE id = $1")
        .bind(row.id)
        .execute(h.store.pool())
        .await
        .unwrap();
    h.fill().await;
    assert!(h.gaps().await.is_empty());
    assert_eq!(h.stored("p").await, range(0, 35));
    failures.remove().await;
    h.finish().await;
}

/// Review finding: a hole whose insert the database refuses, while the
/// handler waits, is kept exactly and retried, even though its batch is not
/// stored yet when it is first refused. Nothing falls back to a probe that
/// could run before the batch is visible.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_refused_hole_is_kept_and_recorded_exactly_on_retry() {
    let mut h = Harness::with_recorder(&["f"]).await;
    h.append("f", 5);
    h.sync(&[deliver("f", 0, 5)]).await;
    h.append("f", 30);
    // Every attempt the handler waits for is refused (many more than fit in
    // the budget), and so is the first retry from memory.
    let failures = InjectedFailures::install(
        &h.store,
        "room_timeline_gaps",
        &format!("NEW.room_id = '{}'", h.room("f")),
        1_000,
    )
    .await;
    h.sync(&[deliver("f", 30, 35).limited()]).await;
    let in_handler = failures.attempts().await;
    assert!(in_handler > 1, "retried while the handler waited");
    assert!(h.gaps().await.is_empty(), "refused");
    assert_eq!(h.stored("f").await, &range(0, 5) | &range(30, 35));

    // A second hole meanwhile queues behind it without another budget's
    // wait: the gap table is known to be refusing inserts.
    h.append("f", 10);
    let started = std::time::Instant::now();
    h.sync(&[deliver("f", 40, 45).limited()]).await;
    assert!(
        started.elapsed() < INSERT_BUDGET_FOR_TESTS,
        "queued at once"
    );
    assert_eq!(failures.attempts().await, in_handler, "not attempted");

    // The drain task's next pass, once the database accepts the rows again
    // and the retry delay has passed.
    sqlx_core::query::query(&format!(
        "ALTER SEQUENCE {} RESTART WITH 2000",
        failures.name
    ))
    .execute(h.store.pool())
    .await
    .unwrap();
    tokio::time::sleep(PENDING_RETRY + Duration::from_millis(100)).await;
    let joined = h.joined_rooms();
    h.recorder.as_ref().unwrap().drain(&joined).await;
    let mut rows = h.gaps().await;
    assert_eq!(rows.len(), 2, "both recorded on retry");
    let newer = rows.remove(0);
    assert_eq!(newer.from_token.as_deref(), Some("s40"));
    let row = rows.remove(0);
    assert_eq!(row.origin, GapOrigin::Limited);
    assert_eq!(row.from_token.as_deref(), Some("s30"));
    failures.remove().await;

    h.fill().await;
    assert!(h.gaps().await.is_empty());
    assert_eq!(h.stored("f").await, range(0, 45));
    h.finish().await;
}

/// Review finding: a hole's row must be committed before the SDK saves its
/// sync position past the response, or a restart forgets the hole. The
/// handler drains the recorder, so while the insert waits on a lock the
/// response is not finished, and the row exists the moment it is.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_hole_is_committed_before_the_response_finishes() {
    let mut h = Harness::with_recorder(&["g"]).await;
    h.append("g", 5);
    h.sync(&[deliver("g", 0, 5)]).await;
    h.append("g", 30);

    // Block inserts into the gap table, as a lock held elsewhere would.
    let mut blocker = h.store.pool().begin().await.unwrap();
    sqlx_core::query::query("LOCK TABLE room_timeline_gaps IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    h.queue(&[deliver("g", 30, 35).limited()]);
    let sync = h.sync.clone();
    let response = tokio::spawn(async move { sync.sync_once().await });
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        !response.is_finished(),
        "the response finished, and its position could be saved, before its hole was recorded"
    );
    blocker.rollback().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), response)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    h.updates.try_recv().expect("the response's broadcast");
    let row = h.gaps().await.pop().expect("committed with the response");
    assert_eq!(row.from_token.as_deref(), Some("s30"));
    h.finish().await;
}

/// The same rule for ordinary backfill, which shares the page writer: a page
/// with a refused write is a failure, and the cursor is not saved past it.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn ordinary_backfill_keeps_its_cursor_when_a_write_fails() {
    let mut h = Harness::new(&["o"]).await;
    h.append("o", 12);
    h.sync(&[deliver("o", 8, 12).limited()]).await;
    let failures = InjectedFailures::install(
        &h.store,
        "events",
        &format!(
            "NEW.room_id = '{}' AND NEW.event_id = '{}'",
            h.room("o"),
            Harness::event_id("o", 5)
        ),
        1,
    )
    .await;
    // The first page from the live end holds 8..11, all stored already.
    assert!(matches!(h.backfill_page("o").await, PageOutcome::Paged));
    let before = h
        .store
        .get_room_backfill(h.account_id, h.room("o"))
        .await
        .unwrap()
        .unwrap()
        .oldest_seen_token;
    assert_eq!(before.as_deref(), Some("s8"));
    assert!(matches!(
        h.backfill_page("o").await,
        PageOutcome::Failed(PageFailure::StoreWrite)
    ));
    let after = h
        .store
        .get_room_backfill(h.account_id, h.room("o"))
        .await
        .unwrap()
        .unwrap()
        .oldest_seen_token;
    assert_eq!(after, before, "the cursor did not move past 5");
    failures.remove().await;
    h.finish().await;
}

/// Review finding (#1143): an event the store refuses on every attempt is
/// skipped, as one that does not parse is, so it cannot pin the cursor.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn ordinary_backfill_skips_an_event_the_store_can_never_accept() {
    let mut h = Harness::new(&["n"]).await;
    h.append("n", 12);
    h.sync(&[deliver("n", 8, 12).limited()]).await;
    let failures = InjectedFailures::install_with(
        &h.store,
        "events",
        "INSERT",
        PERMANENT,
        &format!(
            "NEW.room_id = '{}' AND NEW.event_id = '{}'",
            h.room("n"),
            Harness::event_id("n", 5)
        ),
        u32::MAX,
    )
    .await;
    assert!(matches!(h.backfill_page("n").await, PageOutcome::Paged));
    assert!(matches!(h.backfill_page("n").await, PageOutcome::Paged));
    let state = h
        .store
        .get_room_backfill(h.account_id, h.room("n"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        state.oldest_seen_token.as_deref(),
        Some("s4"),
        "moved past 5"
    );
    assert_eq!(state.consecutive_failures, 0);
    assert_eq!(failures.attempts().await, 1, "tried once");
    assert_eq!(h.stored("n").await, &range(4, 5) | &range(6, 12));
    failures.remove().await;
    h.finish().await;
}
