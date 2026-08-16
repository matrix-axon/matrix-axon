//! In-process Axum stub of the Axon `/v1/` API for the `stub` profile.
//!
//! It implements only the S1 TUI contract — account list, room list, room
//! timeline, message send, and `/v1/ws` — returning handwritten wire-compatible
//! JSON. Every request is recorded in a journal, and a successful send
//! broadcasts a run-marked `timeline.event` echo over the WebSocket so the TUI's
//! live path renders it. Login and the remaining mutations are deferred to S2.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::sync::{broadcast, oneshot};
use uuid::Uuid;

use crate::wire::{
    AccountDto, ApiResponse, EventDto, RoomDto, SendRequest, SendResultDto, ThreadSummaryDto,
    TimelinePage, WsEnvelope,
};

/// One recorded HTTP request, for post-hoc journal assertions.
#[derive(Clone, Debug)]
pub struct JournalEntry {
    pub method: String,
    pub path: String,
    pub body: Option<serde_json::Value>,
}

/// Shared, cheaply cloneable stub state handed to every handler and to the
/// scenario for assertions.
#[derive(Clone)]
pub struct StubState {
    pub account_id: Uuid,
    pub user_id: String,
    pub room_id: String,
    pub room_name: String,
    pub run_id: String,
    pub extra_seed_bodies: Vec<String>,
    /// Thread summaries returned by `GET .../rooms/{room_id}/threads`.
    pub thread_summaries: Vec<ThreadSummaryDto>,
    journal: Arc<Mutex<Vec<JournalEntry>>>,
    /// Hands out `arrival_order` for live sends. Seeded above the highest value
    /// `get_timeline` derives, so an echo is always newer than the whole page.
    send_arrival_order: Arc<AtomicI64>,
    ws_tx: broadcast::Sender<String>,
}

impl StubState {
    fn new(
        run_id: String,
        extra_seed_bodies: Vec<String>,
        thread_summaries: Vec<ThreadSummaryDto>,
    ) -> Self {
        let (ws_tx, _) = broadcast::channel(64);
        let extra_seed_count = extra_seed_bodies.len() as i64;
        Self {
            account_id: Uuid::new_v4(),
            user_id: "@smoke:localhost".to_owned(),
            room_id: "!smoke:localhost".to_owned(),
            room_name: "Smoke Room".to_owned(),
            run_id,
            extra_seed_bodies,
            thread_summaries,
            journal: Arc::new(Mutex::new(Vec::new())),
            send_arrival_order: Arc::new(AtomicI64::new(extra_seed_count + 2)),
            ws_tx,
        }
    }

    /// Next `arrival_order` for a live send: strictly greater than every value
    /// `get_timeline` derives, and increasing per send.
    fn next_arrival_order(&self) -> i64 {
        self.send_arrival_order.fetch_add(1, Ordering::Relaxed)
    }

    /// Snapshot of every request the stub has served so far.
    pub fn journal(&self) -> Vec<JournalEntry> {
        self.journal.lock().expect("journal lock").clone()
    }

    fn record(&self, method: &str, path: &str, body: Option<serde_json::Value>) {
        self.journal
            .lock()
            .expect("journal lock")
            .push(JournalEntry {
                method: method.to_owned(),
                path: path.to_owned(),
                body,
            });
    }

    fn account(&self) -> AccountDto {
        AccountDto {
            account_id: self.account_id,
            user_id: self.user_id.clone(),
            homeserver_url: "https://localhost".to_owned(),
            device_id: Some("SMOKEDEVICE".to_owned()),
            state: "active",
            verified: None,
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    fn room(&self) -> RoomDto {
        RoomDto {
            account_id: self.account_id,
            account_user_id: self.user_id.clone(),
            room_id: self.room_id.clone(),
            name: Some(self.room_name.clone()),
            topic: None,
            avatar_url: None,
            canonical_alias: None,
            last_activity_ts: now_ms(),
            last_event_id: Some(seed_event_id(&self.run_id)),
        }
    }

    /// A message event in this room from the configured sender.
    ///
    /// `arrival_order` is the caller's business: it must increase with ingest
    /// order, and the newest event in a page must hold the greatest value
    /// (ADR 0089), which is the opposite of this stub's newest-first page
    /// order. Callers therefore compute it rather than letting this helper
    /// guess.
    fn message_event(&self, event_id: String, body: String, arrival_order: i64) -> EventDto {
        EventDto {
            account_id: self.account_id,
            event_id,
            room_id: self.room_id.clone(),
            sender: self.user_id.clone(),
            state_key: None,
            arrival_order,
            origin_ts: now_ms(),
            event_type: "m.room.message".to_owned(),
            content: Some(serde_json::json!({ "msgtype": "m.text", "body": body })),
            body: Some(body),
            relates_to: None,
            redacted: false,
            redaction_event_id: None,
        }
    }
}

/// A running stub: its loopback address plus a handle to shut it down.
pub struct Stub {
    pub addr: SocketAddr,
    pub state: StubState,
    shutdown: Option<oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Stub {
    /// Bind an ephemeral loopback port and serve the stub until dropped.
    pub async fn start(run_id: &str) -> anyhow::Result<Self> {
        Self::bind(StubState::new(run_id.to_owned(), vec![], vec![])).await
    }

    /// Like [`Stub::start`] but seeds additional message bodies into the
    /// timeline response.
    pub async fn start_with_seeds(
        run_id: &str,
        extra_seed_bodies: Vec<String>,
    ) -> anyhow::Result<Self> {
        Self::bind(StubState::new(run_id.to_owned(), extra_seed_bodies, vec![])).await
    }

    /// Like [`Stub::start_with_seeds`] but also seeds thread summaries returned
    /// by `GET .../rooms/{room_id}/threads`.  Each entry is `(root_event_id,
    /// reply_count)`.  Thread summaries trigger `apply_relation_outcome` in the
    /// TUI, which under the `scroll_follow_tail` fix must not advance the
    /// viewport.
    pub async fn start_with_seeds_and_threads(
        run_id: &str,
        extra_seed_bodies: Vec<String>,
        thread_summaries: Vec<(String, i64)>,
    ) -> anyhow::Result<Self> {
        let summaries = thread_summaries
            .into_iter()
            .map(|(root_event_id, reply_count)| ThreadSummaryDto {
                root_event_id,
                reply_count,
            })
            .collect();
        Self::bind(StubState::new(
            run_id.to_owned(),
            extra_seed_bodies,
            summaries,
        ))
        .await
    }

    async fn bind(state: StubState) -> anyhow::Result<Self> {
        let app = stub_router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (shutdown, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        Ok(Self {
            addr,
            state,
            shutdown: Some(shutdown),
            handle,
        })
    }

    /// Base URL the TUI should be pointed at via `--base-url`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stop serving and await the server task.
    pub async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.handle.await;
    }
}

fn stub_router(state: StubState) -> Router {
    Router::new()
        .route("/v1/accounts", get(get_accounts))
        .route("/v1/rooms", get(get_rooms))
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/timeline",
            get(get_timeline),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/threads",
            get(get_threads),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/send",
            post(post_send),
        )
        .route("/v1/ws", get(ws_handler))
        .with_state(state)
}

async fn get_accounts(State(state): State<StubState>) -> impl IntoResponse {
    state.record("GET", "/v1/accounts", None);
    Json(ApiResponse {
        data: vec![state.account()],
    })
}

async fn get_rooms(State(state): State<StubState>) -> impl IntoResponse {
    state.record("GET", "/v1/rooms", None);
    Json(ApiResponse {
        data: vec![state.room()],
    })
}

async fn get_timeline(
    State(state): State<StubState>,
    Path((account_id, room_id)): Path<(String, String)>,
) -> impl IntoResponse {
    state.record(
        "GET",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/timeline"),
        None,
    );
    // The page is newest-first and `arrival_order` must be greatest for the
    // newest, so count down from the page size. Values are stable across
    // requests because they are derived from position, not a counter.
    let newest = state.extra_seed_bodies.len() as i64 + 1;
    let seed = state.message_event(
        seed_event_id(&state.run_id),
        format!("smoke seed {}", state.run_id),
        newest,
    );
    let mut events = vec![seed];
    for (i, body) in state.extra_seed_bodies.iter().enumerate() {
        events.push(state.message_event(
            format!("$seed-extra-{i}-{}", state.run_id),
            body.clone(),
            newest - 1 - i as i64,
        ));
    }
    Json(ApiResponse {
        data: TimelinePage {
            events,
            next_cursor: None,
        },
    })
}

async fn get_threads(
    State(state): State<StubState>,
    Path((account_id, room_id)): Path<(String, String)>,
) -> impl IntoResponse {
    state.record(
        "GET",
        &format!("/v1/accounts/{account_id}/rooms/{room_id}/threads"),
        None,
    );
    Json(ApiResponse {
        data: state.thread_summaries.clone(),
    })
}

async fn post_send(
    State(state): State<StubState>,
    Path((account_id, room_id)): Path<(String, String)>,
    Json(request): Json<SendRequest>,
) -> impl IntoResponse {
    let path = format!("/v1/accounts/{account_id}/rooms/{room_id}/send");
    state.record(
        "POST",
        &path,
        Some(serde_json::json!({ "body": request.body })),
    );

    let event_id = format!("$smoke-{}", Uuid::new_v4());
    let echo = state.message_event(event_id.clone(), request.body, state.next_arrival_order());
    let envelope = WsEnvelope {
        kind: "timeline.event".to_owned(),
        account_id: state.account_id,
        payload: echo,
    };
    if let Ok(frame) = serde_json::to_string(&envelope) {
        // Best effort: a send before the TUI's WS connects simply has no
        // subscribers yet; the scenario connects first, so this is delivered.
        let _ = state.ws_tx.send(frame);
    }

    Json(ApiResponse {
        data: SendResultDto { event_id },
    })
}

async fn ws_handler(
    State(state): State<StubState>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    state.record("GET", "/v1/ws", None);
    let mut rx = state.ws_tx.subscribe();
    upgrade.on_upgrade(move |socket| async move {
        let mut socket: WebSocket = socket;
        loop {
            tokio::select! {
                frame = rx.recv() => match frame {
                    Ok(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                incoming = socket.recv() => match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                },
            }
        }
    })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn seed_event_id(run_id: &str) -> String {
    format!("$seed-{run_id}")
}
