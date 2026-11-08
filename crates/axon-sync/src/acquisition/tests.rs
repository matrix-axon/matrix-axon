use super::*;
use axum::{body::Body, http::HeaderMap, response::IntoResponse, Json, Router};
use matrix_sdk::{
    authentication::matrix::MatrixSession, ruma::room_id, store::RoomLoadSettings, SessionMeta,
    SessionTokens,
};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};

fn status(status: u16, errcode: Option<&str>) -> Reply {
    Reply::Status {
        status,
        errcode: errcode.map(str::to_owned),
        retry_after_ms: None,
    }
}

fn label(kind: DetailKind, reply: Reply) -> &'static str {
    outcome_label(&interpret(kind, reply))
}

#[test]
fn replies_map_to_the_documented_outcomes() {
    use DetailKind::*;
    let not_found = || status(404, Some("M_NOT_FOUND"));
    let unrecognized = || status(404, Some("M_UNRECOGNIZED"));
    for kind in [LocalAliases, DirectoryVisibility] {
        assert_eq!(label(kind, status(403, Some("M_FORBIDDEN"))), "forbidden");
        assert_eq!(label(kind, not_found()), "not_found");
        assert_eq!(label(kind, unrecognized()), "unsupported");
        assert_eq!(
            label(kind, status(405, Some("M_UNRECOGNIZED"))),
            "unsupported"
        );
        assert_eq!(label(kind, status(405, None)), "unsupported");
        assert_eq!(
            label(kind, status(404, None)),
            "unsupported",
            "a bare 404 is a missing endpoint, from the server or a proxy"
        );
        assert_eq!(label(kind, Reply::TooLarge), "too_large");
        assert_eq!(
            label(kind, Reply::Ok(b"not json".to_vec())),
            "invalid_response"
        );
    }
    assert_eq!(label(Absence, not_found()), "absent");
    assert_eq!(label(Absence, Reply::Ok(Vec::new())), "present");
    assert_eq!(label(Absence, status(403, None)), "forbidden");
    assert_eq!(label(Absence, unrecognized()), "invalid_response");
    assert_eq!(label(Absence, status(404, None)), "invalid_response");
    assert_eq!(label(Absence, Reply::TooLarge), "invalid_response");
    for kind in [LocalAliases, DirectoryVisibility, Absence] {
        assert_eq!(
            label(kind, status(401, Some("M_UNKNOWN_TOKEN"))),
            "unauthorized"
        );
        assert_eq!(label(kind, status(429, None)), "rate_limited");
        assert_eq!(label(kind, status(502, None)), "server_error");
        assert_eq!(label(kind, status(400, None)), "invalid_response");
        assert_eq!(label(kind, Reply::Failed(NoAnswer::Timeout)), "timeout");
        assert_eq!(label(kind, Reply::Failed(NoAnswer::Transport)), "transport");
        assert_eq!(
            label(kind, Reply::Failed(NoAnswer::Unauthorized)),
            "unauthorized"
        );
    }
    // Expirations follow the ADR's table.
    let ttl = |kind, reply| match interpret(kind, reply) {
        DetailOutcome::Answer { ttl_ms, .. } => ttl_ms,
        other => panic!("{other:?}"),
    };
    assert_eq!(ttl(Absence, not_found()), ms(ABSENCE_TTL));
    assert_eq!(ttl(LocalAliases, not_found()), ms(CONCLUSIVE_DENIAL_TTL));
    assert_eq!(ttl(LocalAliases, unrecognized()), ms(UNSUPPORTED_TTL));
    assert_eq!(ttl(LocalAliases, Reply::TooLarge), ms(AVAILABLE_TTL));
    match interpret(
        LocalAliases,
        Reply::Status {
            status: 429,
            errcode: Some("M_LIMIT_EXCEEDED".into()),
            retry_after_ms: Some(90_000),
        },
    ) {
        DetailOutcome::Failure {
            retry_after_ms,
            min_backoff_ms,
            max_backoff_ms,
            ..
        } => {
            assert_eq!(retry_after_ms, Some(90_000));
            assert_eq!((min_backoff_ms, max_backoff_ms), (60_000, 1_800_000));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn alias_lists_are_validated_and_bounded() {
    let reply = |body: Value| Reply::Ok(serde_json::to_vec(&body).unwrap());
    let value = |outcome| match outcome {
        DetailOutcome::Answer { status, value, .. } => (status, value),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        value(interpret(
            DetailKind::LocalAliases,
            reply(json!({"aliases": ["#a:localhost", "#b:localhost"]}))
        )),
        ("available", Some(json!(["#a:localhost", "#b:localhost"])))
    );
    assert_eq!(
        value(interpret(
            DetailKind::LocalAliases,
            reply(json!({"aliases": []}))
        )),
        ("available", Some(json!([])))
    );
    assert_eq!(
        value(interpret(
            DetailKind::LocalAliases,
            reply(json!({"aliases": ["#a:localhost", "not-an-alias", 7]}))
        )),
        ("partial", Some(json!(["#a:localhost"])))
    );
    let many: Vec<String> = (0..=MAX_LOCAL_ALIASES)
        .map(|i| format!("#a{i}:localhost"))
        .collect();
    assert_eq!(
        value(interpret(
            DetailKind::LocalAliases,
            reply(json!({"aliases": many}))
        )),
        ("too_large", None),
        "an oversized list is withheld, not cut"
    );
    assert_eq!(
        label(
            DetailKind::LocalAliases,
            reply(json!({"aliases": "#a:localhost"}))
        ),
        "invalid_response"
    );
    assert_eq!(
        value(interpret(
            DetailKind::DirectoryVisibility,
            reply(json!({"visibility": "public"}))
        )),
        ("available", Some(json!({"visibility": "public"})))
    );
    assert_eq!(
        label(
            DetailKind::DirectoryVisibility,
            reply(json!({"visibility": "secret"}))
        ),
        "invalid_response"
    );
}

fn changes() -> RoomDetailsChanges {
    let (live_tx, _) = tokio::sync::broadcast::channel(8);
    RoomDetailsChanges::new(Uuid::new_v4(), live_tx)
}

#[test]
fn the_queue_coalesces_and_drops_its_oldest_job_when_full() {
    let changes = changes();
    let queue = Queue::new(changes.clone());
    let room = |i: usize| OwnedRoomId::try_from(format!("!r{i}:localhost")).unwrap();
    assert_eq!(queue.push(room(0), RoomDetailJob::Absence), None);
    assert_eq!(queue.push(room(0), RoomDetailJob::Absence), None);
    assert_eq!(queue.depth(), 1, "a duplicate coalesces");
    assert_eq!(queue.push(room(0), RoomDetailJob::LocalAliases), None);
    assert_eq!(queue.depth(), 2, "each kind has its own entry");
    for i in 1..QUEUE_CAPACITY - 1 {
        assert_eq!(queue.push(room(i), RoomDetailJob::Absence), None);
    }
    assert_eq!(queue.depth(), QUEUE_CAPACITY);
    // A panel shows the oldest job queued.
    assert_eq!(
        queue.observe("!r0:localhost", RoomDetailJob::Absence),
        JobActivity::Queued
    );
    assert_eq!(
        queue.push(room(99), RoomDetailJob::Absence),
        Some((room(0), RoomDetailJob::Absence)),
        "the oldest job makes way for the newest"
    );
    assert_eq!(queue.depth(), QUEUE_CAPACITY);
    assert_eq!(
        changes.take_pending().remove("!r0:localhost"),
        Some(std::collections::BTreeSet::from([
            RoomDetailSource::Metadata
        ])),
        "a dropped job a read saw is announced, so its panel re-reads"
    );
    assert_eq!(
        queue.push(room(98), RoomDetailJob::Absence),
        Some((room(0), RoomDetailJob::LocalAliases))
    );
    assert!(
        changes.take_pending().is_empty(),
        "an unseen drop needs no frame"
    );
    assert_eq!(
        queue.observe("!r0:localhost", RoomDetailJob::Absence),
        JobActivity::Idle
    );
    assert_eq!(
        queue.observe("!r99:localhost", RoomDetailJob::Absence),
        JobActivity::Queued
    );
}

#[tokio::test]
async fn a_running_job_is_not_queued_again_and_reads_are_remembered() {
    let queue = Queue::default();
    let room = room_id!("!r:localhost").to_owned();
    queue.push(room.clone(), RoomDetailJob::DirectoryVisibility);
    assert_eq!(
        queue.next().await,
        (room.clone(), RoomDetailJob::DirectoryVisibility)
    );
    assert_eq!(
        queue.push(room.clone(), RoomDetailJob::DirectoryVisibility),
        None
    );
    assert_eq!(queue.depth(), 0, "the running job answers this read too");
    assert!(!queue.finish(false), "nobody read it");
    queue.push(room.clone(), RoomDetailJob::DirectoryVisibility);
    assert_eq!(
        queue.observe("!r:localhost", RoomDetailJob::DirectoryVisibility),
        JobActivity::Queued
    );
    queue.next().await;
    assert_eq!(
        queue.observe("!r:localhost", RoomDetailJob::DirectoryVisibility),
        JobActivity::Queued,
        "taken but waiting for its interval or a gate permit"
    );
    queue.sending(true);
    assert_eq!(
        queue.observe("!r:localhost", RoomDetailJob::DirectoryVisibility),
        JobActivity::Running
    );
    queue.sending(false);
    assert_eq!(
        queue.observe("!r:localhost", RoomDetailJob::DirectoryVisibility),
        JobActivity::Queued,
        "between an absence job's requests"
    );
    assert!(queue.finish(false), "a read saw it");
    assert_eq!(
        queue.observe("!r:localhost", RoomDetailJob::DirectoryVisibility),
        JobActivity::Idle
    );
}

/// A job that could not read or record the cache stored no backoff, so the
/// queue holds it back; otherwise its observer's re-read would requeue it at
/// once and send another request.
#[tokio::test(start_paused = true)]
async fn a_local_failure_holds_the_job_back_for_a_minute() {
    let queue = Queue::default();
    let room = room_id!("!r:localhost").to_owned();
    queue.push(room.clone(), RoomDetailJob::Absence);
    queue.next().await;
    queue.finish(true);
    queue.push(room.clone(), RoomDetailJob::Absence);
    queue.push(room.clone(), RoomDetailJob::LocalAliases);
    assert_eq!(queue.depth(), 1, "only the failed job is held back");
    assert_eq!(
        queue.observe("!r:localhost", RoomDetailJob::Absence),
        JobActivity::Idle
    );
    tokio::time::advance(FAILURE_BACKOFF_MIN).await;
    queue.push(room.clone(), RoomDetailJob::Absence);
    assert_eq!(queue.depth(), 2);
    // The hold-back list is bounded.
    for i in 0..COOLDOWN_CAPACITY + 5 {
        let room = OwnedRoomId::try_from(format!("!c{i}:localhost")).unwrap();
        queue.lock().running = Some(Entry {
            room_id: room,
            job: RoomDetailJob::Absence,
            observed: false,
        });
        queue.finish(true);
    }
    assert_eq!(queue.lock().cooldown.len(), COOLDOWN_CAPACITY);
}

#[test]
fn retry_after_accepts_seconds_and_http_dates() {
    let now = chrono::DateTime::parse_from_rfc2822("Tue, 03 Nov 2026 12:00:00 GMT")
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert_eq!(retry_after(" 120 ", now), Some(120_000));
    assert_eq!(
        retry_after("Tue, 03 Nov 2026 12:02:30 GMT", now),
        Some(150_000)
    );
    assert_eq!(
        retry_after("Tue, 03 Nov 2026 11:00:00 GMT", now),
        Some(0),
        "a past date is no delay"
    );
    assert_eq!(retry_after("soon", now), None);
    match interpret(
        DetailKind::LocalAliases,
        Reply::Status {
            status: 401,
            errcode: Some("M_UNKNOWN_TOKEN".into()),
            retry_after_ms: Some(30_000),
        },
    ) {
        DetailOutcome::Failure {
            category,
            retry_after_ms,
            ..
        } => assert_eq!((category, retry_after_ms), ("unauthorized", Some(30_000))),
        other => panic!("{other:?}"),
    }
}

/// A request waiting for a gate permit has not been admitted, so its job
/// still reads as queued (ADR 0126 decision 3).
#[tokio::test]
async fn a_request_is_admitted_only_with_a_gate_permit() {
    let (address, _) = mock_homeserver().await;
    let gate = HomeserverGate::for_test();
    gate.open_for_test();
    let homeserver =
        Arc::new(Homeserver::new(sdk_client(address).await, Arc::clone(&gate)).unwrap());
    let admitted = Arc::new(AtomicBool::new(false));
    let waiting = tokio::spawn({
        let (homeserver, admitted) = (Arc::clone(&homeserver), Arc::clone(&admitted));
        async move {
            homeserver
                .get(
                    Get::detail(&["rooms", "!r:localhost", "aliases"], true),
                    &|| admitted.store(true, Ordering::SeqCst),
                )
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(gate.snapshot().waiting, 1, "parked at the gate");
    assert!(!admitted.load(Ordering::SeqCst));
    waiting.abort();
}

#[test]
fn the_registry_ignores_stopped_accounts_and_keeps_a_newer_run() {
    let registry = RoomDetailAcquisitions::default();
    let account = Uuid::new_v4();
    registry.request(account, "!r:localhost", RoomDetailJob::Absence);
    assert_eq!(
        registry.activity(account, "!r:localhost", RoomDetailJob::Absence),
        JobActivity::Idle
    );
    let first = registry.register(account, changes());
    registry.request(account, "!r:localhost", RoomDetailJob::Absence);
    registry.request(account, "not a room id", RoomDetailJob::Absence);
    assert_eq!(first.queue().depth(), 1);
    assert_eq!(
        registry.activity(account, "!r:localhost", RoomDetailJob::Absence),
        JobActivity::Queued
    );
    // A restart starts with nothing queued.
    let second = registry.register(account, changes());
    drop(first);
    assert_eq!(
        registry.activity(account, "!r:localhost", RoomDetailJob::Absence),
        JobActivity::Idle
    );
    registry.request(account, "!r:localhost", RoomDetailJob::Absence);
    assert_eq!(
        second.queue().depth(),
        1,
        "the older run's drop kept the newer queue"
    );
    drop(second);
    assert!(registry.queue(account).is_none());
}

async fn sdk_client(address: std::net::SocketAddr) -> Client {
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
                    user_id: "@acquire:localhost".parse().unwrap(),
                    device_id: "ACQUIRE".into(),
                },
                tokens: SessionTokens {
                    access_token: "fixture-token".into(),
                    refresh_token: None,
                },
            },
            RoomLoadSettings::default(),
        )
        .await
        .unwrap();
    client
}

/// A mock homeserver: each path suffix gets its own canned answer.
async fn mock_homeserver() -> (std::net::SocketAddr, Arc<AtomicBool>) {
    let unauthorized = Arc::new(AtomicBool::new(false));
    let app = Router::new().fallback({
        let unauthorized = unauthorized.clone();
        move |uri: axum::http::Uri, headers: HeaderMap| {
            let unauthorized = unauthorized.clone();
            async move {
                if headers.get("authorization").and_then(|v| v.to_str().ok())
                    != Some("Bearer fixture-token")
                {
                    unauthorized.store(true, Ordering::SeqCst);
                }
                let path = uri.path().to_owned();
                if path == "/_matrix/client/v1/rooms/!s:localhost/hierarchy" {
                    Json(json!({"rooms": [], "query": uri.query()})).into_response()
                } else if path.ends_with("/rooms/!r:localhost/aliases") {
                    Json(json!({"aliases": ["#a:localhost"]})).into_response()
                } else if path.ends_with("/directory/list/room/!big:localhost") {
                    // Declared too large: refused before any of it is read.
                    vec![b' '; MAX_RESPONSE_BYTES + 1].into_response()
                } else if path.ends_with("/directory/list/room/!stream:localhost") {
                    // No length declared: refused once the limit is passed.
                    let chunks = (0..20).map(|_| Ok::<_, std::io::Error>(vec![b' '; 8 * 1024]));
                    Body::from_stream(futures_util::stream::iter(chunks)).into_response()
                } else if path.ends_with("/state/m.room.encryption/") {
                    (
                        axum::http::StatusCode::NOT_FOUND,
                        Json(json!({"errcode": "M_NOT_FOUND", "error": "Event not found"})),
                    )
                        .into_response()
                } else if path.ends_with("/state/m.room.server_acl/") {
                    (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        [("retry-after", "120")],
                        Json(json!({"errcode": "M_LIMIT_EXCEEDED", "retry_after_ms": 5000})),
                    )
                        .into_response()
                } else if path.ends_with("/hang") {
                    std::future::pending::<()>().await;
                    unreachable!()
                } else {
                    (
                        axum::http::StatusCode::NOT_FOUND,
                        Json(json!({"errcode": "M_UNRECOGNIZED"})),
                    )
                        .into_response()
                }
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (address, unauthorized)
}

#[tokio::test]
async fn the_homeserver_gets_the_token_and_bodies_are_bounded() {
    let (address, unauthorized) = mock_homeserver().await;
    let homeserver =
        Homeserver::new(sdk_client(address).await, HomeserverGate::for_test()).unwrap();
    assert_eq!(
        homeserver
            .get(
                Get::detail(&["rooms", "!r:localhost", "aliases"], true),
                &|| {}
            )
            .await,
        Reply::Ok(serde_json::to_vec(&json!({"aliases": ["#a:localhost"]})).unwrap())
    );
    assert!(
        !unauthorized.load(Ordering::SeqCst),
        "the account's token is sent"
    );
    for room in ["!big:localhost", "!stream:localhost"] {
        assert_eq!(
            homeserver
                .get(
                    Get::detail(&["directory", "list", "room", room], true),
                    &|| {}
                )
                .await,
            Reply::TooLarge
        );
    }
    // The absence check reads no body, and the empty state key keeps its
    // trailing slash.
    assert_eq!(
        homeserver
            .get(
                Get::detail(
                    &["rooms", "!r:localhost", "state", "m.room.encryption", ""],
                    false
                ),
                &|| {}
            )
            .await,
        status(404, Some("M_NOT_FOUND"))
    );
    assert_eq!(
        homeserver
            .get(
                Get::detail(
                    &["rooms", "!r:localhost", "state", "m.room.server_acl", ""],
                    false
                ),
                &|| {}
            )
            .await,
        Reply::Status {
            status: 429,
            errcode: Some("M_LIMIT_EXCEEDED".into()),
            retry_after_ms: Some(120_000),
        },
        "the longer of Retry-After and retry_after_ms"
    );
}

/// A hierarchy page goes to the v1 path with its query encoded, and its body
/// limit is the request's own.
#[tokio::test]
async fn a_hierarchy_page_uses_v1_and_encodes_its_query() {
    let (address, _) = mock_homeserver().await;
    let homeserver =
        Homeserver::new(sdk_client(address).await, HomeserverGate::for_test()).unwrap();
    let request = |body_limit| Get {
        version: "v1",
        segments: &["rooms", "!s:localhost", "hierarchy"],
        query: &[("max_depth", "1"), ("from", "a b&c=d")],
        body_limit: Some(body_limit),
        timeout: REQUEST_TIMEOUT,
    };
    let Reply::Ok(body) = homeserver.get(request(64 * 1024), &|| {}).await else {
        panic!("the v1 route answers");
    };
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["query"], "max_depth=1&from=a+b%26c%3Dd");
    assert_eq!(
        homeserver.get(request(8), &|| {}).await,
        Reply::TooLarge,
        "the page's own body limit applies"
    );
}

/// A hung peer costs one request deadline and keeps its gate slot (ADR 0122),
/// so it cannot accumulate abandoned work.
#[tokio::test]
async fn a_hung_homeserver_times_out_and_holds_its_gate_slot() {
    let (address, _) = mock_homeserver().await;
    let gate = HomeserverGate::for_test();
    let homeserver = Homeserver::new(sdk_client(address).await, Arc::clone(&gate)).unwrap();
    tokio::time::pause();
    let started = Instant::now();
    assert_eq!(
        homeserver.get(Get::detail(&["hang"], true), &|| {}).await,
        Reply::Failed(NoAnswer::Timeout)
    );
    // Paused time advances to the deadline, give or take a timer tick.
    let elapsed = started.elapsed();
    assert!(elapsed >= REQUEST_TIMEOUT && elapsed < REQUEST_TIMEOUT + Duration::from_secs(1));
    let snapshot = gate.snapshot();
    assert_eq!((snapshot.active, snapshot.held), (0, 1));
}

// ---------------------------------------------------------------------------
// Against PostgreSQL, with a scripted homeserver
// ---------------------------------------------------------------------------

type Hook = Box<dyn FnOnce() -> futures_util::future::BoxFuture<'static, ()> + Send>;

/// Records each request's path and answers from a script; an unscripted path
/// gets `404 M_NOT_FOUND`. A hook runs once, inside the first request.
#[derive(Default)]
struct Script {
    calls: Mutex<Vec<String>>,
    replies: Mutex<HashMap<String, Reply>>,
    during_request: Mutex<Option<Hook>>,
    hang: AtomicBool,
    /// How long the first request waits for its gate permit.
    admission_delay: Mutex<Option<Duration>>,
    admitted: Mutex<Vec<Instant>>,
    /// What `/versions` advertises; `None` is Matrix v1.15 or later.
    summary_endpoint: Mutex<Option<summary::SummaryEndpoint>>,
    /// How many times the worker read `/versions`.
    versions_reads: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl Upstream for Arc<Script> {
    async fn get(&self, request: Get<'_>, admitted: &(dyn Fn() + Sync)) -> Reply {
        let delay = self.admission_delay.lock().unwrap().take();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        admitted();
        self.admitted.lock().unwrap().push(Instant::now());
        let mut path = request.segments.join("/");
        if !request.query.is_empty() {
            let query: Vec<String> = request
                .query
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect();
            path = format!("{path}?{}", query.join("&"));
        }
        self.calls.lock().unwrap().push(path.clone());
        if self.hang.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        let hook = self.during_request.lock().unwrap().take();
        if let Some(hook) = hook {
            hook().await;
        }
        self.replies
            .lock()
            .unwrap()
            .get(&path)
            .cloned()
            .unwrap_or_else(|| status(404, Some("M_NOT_FOUND")))
    }

    async fn summary_endpoint(&self) -> summary::SummaryEndpoint {
        self.versions_reads.fetch_add(1, Ordering::SeqCst);
        self.summary_endpoint
            .lock()
            .unwrap()
            .unwrap_or(summary::SummaryEndpoint::Stable)
    }
}

impl Script {
    fn take_calls(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }

    fn reply(&self, path: String, reply: Reply) {
        self.replies.lock().unwrap().insert(path, reply);
    }
}

struct Fixture {
    store: Store,
    account_id: Uuid,
    user: String,
    script: Arc<Script>,
    worker: Worker<Arc<Script>>,
    changes: RoomDetailsChanges,
}

const ROOM: &str = "!acquire:localhost";

async fn state(
    store: &Store,
    account_id: Uuid,
    user: &str,
    event_type: &str,
    key: &str,
    content: Value,
) {
    let event_id = format!("$state-{}", Uuid::new_v4());
    store
        .upsert_room_state_for_local_user(
            &axon_store::RoomStateUpsert {
                account_id,
                room_id: ROOM,
                event_type,
                state_key: key,
                event_id: &event_id,
                sender: user,
                origin_ts: chrono::Utc::now().timestamp_millis(),
                content: Some(content),
            },
            Some(user),
        )
        .await
        .unwrap();
}

async fn fixture() -> Fixture {
    let store = Store::connect(&std::env::var("DATABASE_URL").unwrap(), 5)
        .await
        .unwrap();
    let user = format!("@acquire-{}:localhost", Uuid::new_v4());
    let account_id = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .unwrap()
        .account_id;
    sqlx_core::query::query("INSERT INTO room_summaries (account_id, room_id, last_activity_ts, last_event_id, last_event_row_id, last_activity_is_content) VALUES ($1, $2, 1, '$fixture', 1, false)")
        .bind(account_id).bind(ROOM).execute(store.pool()).await.unwrap();
    state(
        &store,
        account_id,
        &user,
        "m.room.member",
        &user,
        json!({"membership": "join"}),
    )
    .await;
    state(
        &store,
        account_id,
        &user,
        "m.room.create",
        "",
        json!({"room_version": "12"}),
    )
    .await;
    let script = Arc::new(Script::default());
    let (live_tx, _) = tokio::sync::broadcast::channel(8);
    let changes = RoomDetailsChanges::new(account_id, live_tx);
    let worker = Worker::new(
        Arc::clone(&script),
        store.clone(),
        account_id,
        changes.clone(),
        Arc::new(Queue::new(changes.clone())),
    );
    Fixture {
        store,
        account_id,
        user,
        script,
        worker,
        changes,
    }
}

impl Fixture {
    /// Queue `job`, optionally let a read see it, and run it.
    async fn run(&mut self, job: RoomDetailJob, observed: bool) {
        self.worker.queue.push(ROOM.try_into().unwrap(), job);
        if observed {
            assert_eq!(self.worker.queue.observe(ROOM, job), JobActivity::Queued);
        }
        let (room_id, job) = self.worker.queue.next().await;
        self.worker.step(&room_id, job).await;
    }

    fn frames(&self) -> Vec<RoomDetailSource> {
        self.changes
            .take_pending()
            .remove(ROOM)
            .map(|sources| sources.into_iter().collect())
            .unwrap_or_default()
    }

    async fn rows(&self, kind: DetailKind) -> Vec<axon_store::DetailRow> {
        self.store
            .room_detail_snapshot(self.account_id, ROOM, &[kind])
            .await
            .unwrap()
            .rows
    }
}

fn absence_path(state_type: &str) -> String {
    format!("rooms/{ROOM}/state/{state_type}/")
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn an_absence_job_confirms_due_types_in_order_and_announces_once() {
    let mut f = fixture().await;
    f.script.reply(
        absence_path("m.room.join_rules"),
        status(403, Some("M_FORBIDDEN")),
    );
    f.script
        .reply(absence_path("m.room.guest_access"), Reply::Ok(Vec::new()));
    f.script
        .reply(absence_path("m.room.power_levels"), status(502, None));
    f.run(RoomDetailJob::Absence, false).await;
    let expected: Vec<String> = ABSENCE_ORDER
        .iter()
        .take_while(|t| **t != "m.room.canonical_alias")
        .map(|t| absence_path(t))
        .collect();
    assert_eq!(
        f.script.take_calls(),
        expected,
        "encryption first, through the failed power-levels check"
    );
    assert_eq!(
        f.frames(),
        [RoomDetailSource::Metadata],
        "one frame for the job"
    );
    let rows = f.rows(DetailKind::Absence).await;
    let row = |t: &str| rows.iter().find(|r| r.state_type == t).unwrap();
    assert_eq!(
        row("m.room.encryption").value_status.as_deref(),
        Some("absent")
    );
    assert_eq!(
        row("m.room.join_rules").value_status.as_deref(),
        Some("forbidden")
    );
    assert_eq!(
        row("m.room.guest_access").value_status,
        None,
        "a 200 confirms nothing"
    );
    assert!(row("m.room.guest_access").next_attempt_at.is_some());
    assert_eq!(
        row("m.room.power_levels").failure.as_deref(),
        Some("server_error")
    );
    assert!(
        rows.iter()
            .all(|r| r.state_type != "m.room.canonical_alias"),
        "the failure ended the job"
    );

    // The next job asks only what is still due: not the failed type, which
    // backs off, and never a type with a tuple.
    f.run(RoomDetailJob::Absence, false).await;
    assert_eq!(
        f.script.take_calls(),
        [absence_path("m.room.canonical_alias")]
    );
    assert_eq!(f.frames(), [RoomDetailSource::Metadata]);

    // Nothing is due now, so a job asks nothing and announces nothing.
    f.run(RoomDetailJob::Absence, false).await;
    assert!(f.script.take_calls().is_empty());
    assert!(f.frames().is_empty());
    // A panel that saw the job queued still hears that it ended.
    f.run(RoomDetailJob::Absence, true).await;
    assert!(f.script.take_calls().is_empty());
    assert_eq!(f.frames(), [RoomDetailSource::Metadata]);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn homeserver_details_are_fetched_once_until_they_expire() {
    let mut f = fixture().await;
    f.script.reply(
        format!("rooms/{ROOM}/aliases"),
        Reply::Ok(serde_json::to_vec(&json!({"aliases": ["#a:localhost"]})).unwrap()),
    );
    f.run(RoomDetailJob::LocalAliases, false).await;
    f.run(RoomDetailJob::DirectoryVisibility, false).await;
    assert_eq!(
        f.script.take_calls(),
        [
            format!("rooms/{ROOM}/aliases"),
            format!("directory/list/room/{ROOM}")
        ]
    );
    assert_eq!(f.frames(), [RoomDetailSource::HomeserverDetails]);
    assert_eq!(
        f.rows(DetailKind::LocalAliases).await[0].value,
        Some(json!(["#a:localhost"]))
    );
    assert_eq!(
        f.rows(DetailKind::DirectoryVisibility).await[0]
            .value_status
            .as_deref(),
        Some("not_found")
    );
    f.run(RoomDetailJob::LocalAliases, false).await;
    assert!(
        f.script.take_calls().is_empty(),
        "fresh details are not asked again"
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn unjoined_rooms_get_no_request() {
    let mut f = fixture().await;
    let user = f.user.clone();
    state(
        &f.store,
        f.account_id,
        &user,
        "m.room.member",
        &user,
        json!({"membership": "leave"}),
    )
    .await;
    for job in [
        RoomDetailJob::Absence,
        RoomDetailJob::LocalAliases,
        RoomDetailJob::DirectoryVisibility,
    ] {
        f.run(job, false).await;
    }
    assert!(f.script.take_calls().is_empty());
    assert!(f.frames().is_empty());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_leave_and_rejoin_during_a_request_commits_nothing() {
    let mut f = fixture().await;
    let (store, account_id, user) = (f.store.clone(), f.account_id, f.user.clone());
    *f.script.during_request.lock().unwrap() = Some(Box::new(move || {
        Box::pin(async move {
            state(
                &store,
                account_id,
                &user,
                "m.room.member",
                &user,
                json!({"membership": "leave"}),
            )
            .await;
            state(
                &store,
                account_id,
                &user,
                "m.room.member",
                &user,
                json!({"membership": "join"}),
            )
            .await;
        })
    }));
    f.run(RoomDetailJob::Absence, false).await;
    assert_eq!(
        f.script.take_calls(),
        [absence_path("m.room.encryption")],
        "a membership change stops the job"
    );
    assert!(
        f.rows(DetailKind::Absence).await.is_empty(),
        "the rejoined room starts empty"
    );
    assert!(f.frames().is_empty());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn state_delivered_during_a_request_wins() {
    let mut f = fixture().await;
    let (store, account_id, user) = (f.store.clone(), f.account_id, f.user.clone());
    *f.script.during_request.lock().unwrap() = Some(Box::new(move || {
        Box::pin(async move {
            state(
                &store,
                account_id,
                &user,
                "m.room.encryption",
                "",
                json!({"algorithm": "m.megolm.v1.aes-sha2"}),
            )
            .await;
        })
    }));
    f.run(RoomDetailJob::Absence, false).await;
    assert_eq!(
        f.script.take_calls().len(),
        7,
        "only its own type is fenced"
    );
    let rows = f.rows(DetailKind::Absence).await;
    assert!(rows.iter().all(|r| r.state_type != "m.room.encryption"));
    assert_eq!(rows.len(), 6);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_hung_peer_holds_only_its_own_account() {
    let mut hung = fixture().await;
    let mut healthy = fixture().await;
    hung.script.hang.store(true, Ordering::SeqCst);
    hung.worker
        .queue
        .push(ROOM.try_into().unwrap(), RoomDetailJob::Absence);
    let (room_id, job) = hung.worker.queue.next().await;
    let stuck = tokio::spawn(async move { hung.worker.step(&room_id, job).await });
    tokio::time::timeout(
        Duration::from_secs(10),
        healthy.run(RoomDetailJob::Absence, false),
    )
    .await
    .expect("another account's worker is unaffected");
    assert_eq!(healthy.script.take_calls().len(), 7);
    assert!(!stuck.is_finished());
    stuck.abort();
}

/// A homeserver that is timing out, rate limiting, or failing gets one
/// request per job, not one per missing type.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_failed_request_ends_an_absence_job() {
    let mut f = fixture().await;
    f.script.reply(
        absence_path("m.room.encryption"),
        Reply::Failed(NoAnswer::Timeout),
    );
    f.run(RoomDetailJob::Absence, false).await;
    assert_eq!(f.script.take_calls(), [absence_path("m.room.encryption")]);
    let due = f
        .store
        .due_absence_types(f.account_id, ROOM, &ABSENCE_ORDER)
        .await
        .unwrap();
    assert_eq!(
        due.len(),
        6,
        "every other missing type is still due: {due:?}"
    );
    assert!(!due.iter().any(|t| t == "m.room.encryption"));
}

/// The interval counts from when a request was sent, so a long wait for a
/// gate permit is not credited to the next request.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn requests_are_paced_from_their_admission() {
    let mut f = fixture().await;
    *f.script.admission_delay.lock().unwrap() = Some(JOB_INTERVAL * 2);
    f.run(RoomDetailJob::Absence, false).await;
    let admitted = f.script.admitted.lock().unwrap().clone();
    assert_eq!(admitted.len(), 7);
    for pair in admitted.windows(2) {
        assert!(
            pair[1] - pair[0] >= JOB_INTERVAL,
            "{:?} between requests",
            pair[1] - pair[0]
        );
    }
}

mod hierarchy;
mod room_summary;
