//! End-to-end tests of browser sign-in (ADR 0117) through the real acquisition
//! engine, lifecycle, and store, against a loopback server standing in for
//! both the homeserver and its authorization server.
//!
//! The device-grant requests and the poll loop have their own exhaustive unit
//! tests in `matrix_oauth_device_grant`; these prove the pieces are joined up:
//! what a client can read at each stage, what lands in the account row, and
//! that the two route families cannot see each other's flows.

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use axon_core::SyncConfig;
use axon_store::{AccountState, Store, StoredAccountSession};
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing_subscriber::fmt::MakeWriter;
use uuid::Uuid;

use crate::{
    MatrixOAuthAcquireEngine, MatrixOAuthAcquireError, MatrixOAuthAcquirePresentation,
    MatrixOAuthBrowserStage, MatrixOAuthBrowserState,
};

const USER_CODE: &str = "WDJB-MJHT";
const DEVICE_CODE: &str = "device-code-secret";
const ACCESS_TOKEN: &str = "access-token-secret";
const REFRESH_TOKEN: &str = "refresh-token-secret";
const SCOPE_DEVICE_PREFIX: &str = "urn:matrix:org.matrix.msc2967.client:device:";

/// What the stand-in server answers. Each test overrides only what it needs.
struct Behaviour {
    /// Extra or replaced discovery metadata fields; `null` removes one.
    metadata: Value,
    /// The device authorization response, with `{base}` replaced by the
    /// server's own origin.
    device_authorization: Value,
    /// Token-endpoint answers in order; once exhausted, the approval.
    token_script: Vec<(StatusCode, Value)>,
    /// The token response that follows the script.
    approval: (StatusCode, Value),
    /// `whoami`'s user ID; `None` echoes the expected one.
    whoami_user: Option<String>,
    /// `whoami`'s device ID; `None` echoes the one named in the scope.
    whoami_device: Option<String>,
    /// How long `whoami` takes, to hold a flow in `finalizing`.
    whoami_delay: Duration,
}

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            metadata: json!({}),
            device_authorization: json!({
                "device_code": DEVICE_CODE,
                "user_code": USER_CODE,
                "verification_uri": "{base}/link",
                "verification_uri_complete": format!("{{base}}/link?code={USER_CODE}"),
                "expires_in": 600,
                "interval": 1
            }),
            token_script: vec![pending()],
            approval: (
                StatusCode::OK,
                json!({
                    "access_token": ACCESS_TOKEN,
                    "refresh_token": REFRESH_TOKEN,
                    "token_type": "Bearer",
                    "expires_in": 300
                }),
            ),
            whoami_user: None,
            whoami_device: None,
            whoami_delay: Duration::ZERO,
        }
    }
}

fn pending() -> (StatusCode, Value) {
    oauth_error("authorization_pending")
}

fn oauth_error(code: &str) -> (StatusCode, Value) {
    (
        StatusCode::BAD_REQUEST,
        json!({ "error": code, "error_description": "description-secret" }),
    )
}

/// Keeps the token endpoint pending for the whole test.
fn never_approved() -> Behaviour {
    Behaviour {
        approval: pending(),
        ..Behaviour::default()
    }
}

struct Server {
    base: String,
    user_id: String,
    behaviour: Behaviour,
    token_script: Mutex<VecDeque<(StatusCode, Value)>>,
    device_requests: Mutex<Vec<String>>,
    token_requests: AtomicUsize,
}

impl Server {
    /// The device ID named in the scope of the one device authorization request.
    fn requested_device_id(&self) -> String {
        let requests = self.device_requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "exactly one device authorization");
        let scope = url::form_urlencoded::parse(requests[0].as_bytes())
            .find(|(name, _)| name == "scope")
            .map(|(_, value)| value.into_owned())
            .expect("scope");
        scope
            .split(' ')
            .find_map(|scope| scope.strip_prefix(SCOPE_DEVICE_PREFIX))
            .expect("device scope")
            .to_owned()
    }
}

async fn versions() -> Json<Value> {
    Json(json!({ "versions": ["v1.15"], "unstable_features": {} }))
}

async fn auth_metadata(State(server): State<Arc<Server>>) -> Json<Value> {
    let base = &server.base;
    let mut metadata = json!({
        "issuer": format!("{base}/"),
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "registration_endpoint": format!("{base}/register"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query", "fragment"],
        "grant_types_supported": [
            "authorization_code",
            "refresh_token",
            "urn:ietf:params:oauth:grant-type:device_code"
        ],
        "revocation_endpoint": format!("{base}/revoke"),
        "code_challenge_methods_supported": ["S256"],
        "device_authorization_endpoint": format!("{base}/device")
    });
    for (key, value) in server.behaviour.metadata.as_object().unwrap() {
        if value.is_null() {
            metadata.as_object_mut().unwrap().remove(key);
        } else {
            metadata[key] = value.clone();
        }
    }
    Json(metadata)
}

async fn device_authorization(State(server): State<Arc<Server>>, body: String) -> Json<Value> {
    server.device_requests.lock().unwrap().push(body);
    let response = server
        .behaviour
        .device_authorization
        .to_string()
        .replace("{base}", &server.base);
    Json(serde_json::from_str(&response).unwrap())
}

async fn token(State(server): State<Arc<Server>>) -> (StatusCode, Json<Value>) {
    server.token_requests.fetch_add(1, Ordering::SeqCst);
    let (status, body) = server
        .token_script
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| server.behaviour.approval.clone());
    (status, Json(body))
}

async fn whoami(State(server): State<Arc<Server>>) -> Json<Value> {
    tokio::time::sleep(server.behaviour.whoami_delay).await;
    Json(json!({
        "user_id": server
            .behaviour
            .whoami_user
            .clone()
            .unwrap_or_else(|| server.user_id.clone()),
        "device_id": server
            .behaviour
            .whoami_device
            .clone()
            .unwrap_or_else(|| server.requested_device_id()),
    }))
}

/// One engine wired to one stand-in server and the test database.
struct Harness {
    engine: MatrixOAuthAcquireEngine,
    store: Store,
    server: Arc<Server>,
    user_id: String,
    data_dir: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn start(behaviour: Behaviour) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        // A loopback server name, so discovery reaches the stand-in over plain
        // HTTP exactly as it reaches a local development homeserver.
        let user_id = format!("@browser-{}:{address}", Uuid::new_v4().simple());
        let server = Arc::new(Server {
            base: format!("http://{address}"),
            user_id: user_id.clone(),
            token_script: Mutex::new(behaviour.token_script.clone().into()),
            behaviour,
            device_requests: Mutex::new(Vec::new()),
            token_requests: AtomicUsize::new(0),
        });
        let app = Router::new()
            .route("/_matrix/client/versions", get(versions))
            .route("/_matrix/client/v1/auth_metadata", get(auth_metadata))
            .route(
                "/register",
                post(|| async { Json(json!({ "client_id": "browser-test-client" })) }),
            )
            .route("/device", post(device_authorization))
            .route("/token", post(token))
            .route("/_matrix/client/v3/account/whoami", get(whoami))
            .with_state(server.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let data_dir =
            std::env::temp_dir().join(format!("axon-browser-sign-in-test-{}", Uuid::new_v4()));
        let config = SyncConfig {
            data_dir: data_dir.clone(),
            store_key: Some("test-key".to_owned()),
            timeline_limit: 1,
            live_event_buffer: 16,
            ..SyncConfig::default()
        };
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
        let store = Store::connect(&database_url, 5)
            .await
            .expect("connect + migrate");
        let lifecycle = crate::lifecycle::tests::lifecycle_with(config.clone()).await;
        let engine = MatrixOAuthAcquireEngine::new(
            store.clone(),
            config,
            lifecycle,
            TaskTracker::new(),
            CancellationToken::new(),
        );
        Self {
            engine,
            store,
            server,
            user_id,
            data_dir,
            task,
        }
    }

    async fn create(&self) -> MatrixOAuthBrowserState {
        self.engine
            .create_browser(&self.user_id)
            .await
            .expect("create browser sign-in")
    }

    /// Poll like a client until the flow reaches `stage`.
    async fn wait_for(
        &self,
        flow_id: Uuid,
        stage: MatrixOAuthBrowserStage,
    ) -> MatrixOAuthBrowserState {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let state = self.engine.get_browser(flow_id).expect("flow is readable");
                if state.stage == stage {
                    return state;
                }
                assert!(
                    !matches!(
                        state.stage,
                        MatrixOAuthBrowserStage::Done
                            | MatrixOAuthBrowserStage::Failed
                            | MatrixOAuthBrowserStage::Cancelled
                    ),
                    "flow ended as {:?} ({:?}) while waiting for {stage:?}",
                    state.stage,
                    state.error_code
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("flow reached the expected stage in time")
    }

    /// Run a fresh flow to failure and return its stable error code.
    async fn fail(&self) -> String {
        let flow = self.create().await;
        let failed = self
            .wait_for(flow.flow_id, MatrixOAuthBrowserStage::Failed)
            .await;
        assert!(failed.verification_uri.is_none() && failed.user_code.is_none());
        assert!(failed.account_id.is_none());
        failed.error_code.expect("a failed flow names its error")
    }

    /// Failure and cancellation clean up after the flow reports terminal, so
    /// wait for the reservation rather than assuming it is already gone.
    async fn wait_until_released(&self) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self
                .store
                .has_matrix_oauth_acquire_for_user(&self.user_id)
                .await
                .unwrap()
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the identity reservation was released");
    }

    fn staging_dirs(&self) -> usize {
        std::fs::read_dir(self.data_dir.join(".matrix-oauth-acquire"))
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    async fn finish(self) {
        self.task.abort();
        if let Some(account) = self
            .store
            .list_accounts()
            .await
            .unwrap()
            .into_iter()
            .find(|account| account.user_id == self.user_id)
        {
            sqlx_core::query::query("DELETE FROM accounts WHERE account_id = $1")
                .bind(account.account_id)
                .execute(self.store.pool())
                .await
                .unwrap();
        }
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for CapturedLogs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedLogs {
    type Writer = CapturedLogs;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The whole path: link and code while waiting, then an active, unverified
/// OAuth account on exactly the device ID the scope named, with nothing secret
/// left on the flow or in the logs.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn approval_activates_an_unverified_account_on_the_requested_device() {
    let captured = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(captured.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let harness = Harness::start(Behaviour {
        whoami_delay: Duration::from_millis(750),
        ..Behaviour::default()
    })
    .await;
    let created = harness.create().await;
    assert_eq!(created.stage, MatrixOAuthBrowserStage::Starting);
    assert_eq!(created.expected_user_id, harness.user_id);
    assert!(created.verification_uri.is_none() && created.user_code.is_none());

    let waiting = harness
        .wait_for(
            created.flow_id,
            MatrixOAuthBrowserStage::WaitingForAuthorization,
        )
        .await;
    assert_eq!(
        waiting.verification_uri.as_deref(),
        Some(format!("{}/link?code={USER_CODE}", harness.server.base).as_str()),
        "the complete link is preferred, so the user need not type the code"
    );
    assert_eq!(waiting.user_code.as_deref(), Some(USER_CODE));

    // The token response alone moves the flow on, before identity validation
    // has answered, and takes the link and code with it.
    let finalizing = harness
        .wait_for(created.flow_id, MatrixOAuthBrowserStage::Finalizing)
        .await;
    assert!(finalizing.verification_uri.is_none() && finalizing.user_code.is_none());
    assert!(finalizing.account_id.is_none());

    let done = harness
        .wait_for(created.flow_id, MatrixOAuthBrowserStage::Done)
        .await;
    assert!(done.verification_uri.is_none() && done.user_code.is_none());
    assert!(done.error_code.is_none());
    let account_id = done.account_id.expect("a completed flow names its account");

    // The script was one `authorization_pending`, then the approval.
    assert_eq!(harness.server.token_requests.load(Ordering::SeqCst), 2);

    let account = harness
        .store
        .get_account(account_id)
        .await
        .unwrap()
        .expect("account row");
    assert_eq!(account.user_id, harness.user_id);
    assert_eq!(account.state, AccountState::Active);
    assert!(
        !account.verified,
        "browser sign-in authenticates; it does not verify the device"
    );
    assert_eq!(
        account.device_id.as_deref(),
        Some(harness.server.requested_device_id().as_str()),
        "the session's device is the one the requested scope named"
    );
    assert_eq!(account.homeserver_url, format!("{}/", harness.server.base));
    match harness
        .store
        .account_session(account_id, "test-key")
        .await
        .unwrap()
        .expect("stored session")
    {
        StoredAccountSession::OAuth {
            access_token,
            refresh_token,
            client_id,
        } => {
            assert_eq!(access_token, ACCESS_TOKEN);
            assert_eq!(refresh_token, REFRESH_TOKEN);
            assert_eq!(client_id, "browser-test-client");
        }
        StoredAccountSession::Matrix { .. } => panic!("expected an OAuth session"),
    }

    // The reservation is consumed and the staged store was adopted.
    assert!(!harness
        .store
        .has_matrix_oauth_acquire_for_user(&harness.user_id)
        .await
        .unwrap());
    assert!(harness.data_dir.join(account_id.to_string()).is_dir());

    // A finished browser flow is still invisible to the QR routes.
    assert!(matches!(
        harness.engine.get(created.flow_id),
        Err(MatrixOAuthAcquireError::NotFound(_))
    ));

    let logs = captured.text();
    assert!(logs.contains("Matrix OAuth login flow completed"));
    assert!(logs.contains("presentation=\"browser\""));
    for secret in [
        USER_CODE,
        DEVICE_CODE,
        ACCESS_TOKEN,
        REFRESH_TOKEN,
        "/link",
        "description-secret",
    ] {
        assert!(!logs.contains(secret), "logs must not contain {secret:?}");
    }
    harness.finish().await;
}

/// One flow per expected Matrix user across all three methods, and each route
/// family sees and cancels only its own flows.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn flows_conflict_across_methods_but_are_isolated_by_route_family() {
    let harness = Harness::start(never_approved()).await;
    let browser = harness.create().await;
    harness
        .wait_for(
            browser.flow_id,
            MatrixOAuthBrowserStage::WaitingForAuthorization,
        )
        .await;

    // A live browser flow refuses every other flow for the same user.
    assert!(matches!(
        harness.engine.create_browser(&harness.user_id).await,
        Err(MatrixOAuthAcquireError::FlowAlreadyExists)
    ));
    for presentation in [
        MatrixOAuthAcquirePresentation::Display,
        MatrixOAuthAcquirePresentation::Scan,
    ] {
        assert!(matches!(
            harness.engine.create(&harness.user_id, presentation).await,
            Err(MatrixOAuthAcquireError::FlowAlreadyExists)
        ));
    }

    // The QR surface cannot read, drive, or cancel the browser flow.
    assert!(matches!(
        harness.engine.get(browser.flow_id),
        Err(MatrixOAuthAcquireError::NotFound(_))
    ));
    assert!(matches!(
        harness.engine.submit_check_code(browser.flow_id, "42"),
        Err(MatrixOAuthAcquireError::NotFound(_))
    ));
    assert!(matches!(
        harness.engine.cancel(browser.flow_id),
        Err(MatrixOAuthAcquireError::NotFound(_))
    ));
    assert_eq!(
        harness.engine.get_browser(browser.flow_id).unwrap().stage,
        MatrixOAuthBrowserStage::WaitingForAuthorization,
        "a cancel through the wrong surface must leave the flow running"
    );

    // Its own surface cancels it, idempotently, and clears the link and code.
    harness.engine.cancel_browser(browser.flow_id).unwrap();
    harness.engine.cancel_browser(browser.flow_id).unwrap();
    let cancelled = harness.engine.get_browser(browser.flow_id).unwrap();
    assert_eq!(cancelled.stage, MatrixOAuthBrowserStage::Cancelled);
    assert!(cancelled.verification_uri.is_none() && cancelled.user_code.is_none());
    harness.wait_until_released().await;
    assert_eq!(harness.staging_dirs(), 0, "the staged store is removed");

    // And the reverse: a QR flow blocks a browser flow and is invisible to
    // the browser surface. A scan flow just waits for input, so it stays live.
    let qr = harness
        .engine
        .create(&harness.user_id, MatrixOAuthAcquirePresentation::Scan)
        .await
        .expect("the user is free again");
    assert!(matches!(
        harness.engine.create_browser(&harness.user_id).await,
        Err(MatrixOAuthAcquireError::FlowAlreadyExists)
    ));
    assert!(matches!(
        harness.engine.get_browser(qr.flow_id),
        Err(MatrixOAuthAcquireError::NotFound(_))
    ));
    assert!(matches!(
        harness.engine.cancel_browser(qr.flow_id),
        Err(MatrixOAuthAcquireError::NotFound(_))
    ));
    assert!(harness.engine.get(qr.flow_id).is_ok());
    harness.engine.cancel(qr.flow_id).unwrap();
    harness.wait_until_released().await;

    // An unknown id stays an idempotent no-op on both surfaces.
    harness.engine.cancel(Uuid::new_v4()).unwrap();
    harness.engine.cancel_browser(Uuid::new_v4()).unwrap();
    harness.finish().await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn refusal_and_expiry_have_their_own_error_codes() {
    let denied = Harness::start(Behaviour {
        token_script: vec![pending(), oauth_error("access_denied")],
        ..Behaviour::default()
    })
    .await;
    assert_eq!(denied.fail().await, "authorization_denied");
    denied.wait_until_released().await;
    denied.finish().await;

    let expired = Harness::start(Behaviour {
        token_script: vec![oauth_error("expired_token")],
        ..Behaviour::default()
    })
    .await;
    assert_eq!(expired.fail().await, "authorization_expired");
    expired.finish().await;

    // A device code that outlives nothing: the server's own `expires_in`
    // ends the wait before the flow's thirty minutes do.
    let mut short_lived = never_approved();
    short_lived.device_authorization["expires_in"] = json!(1);
    let short_lived = Harness::start(short_lived).await;
    assert_eq!(short_lived.fail().await, "authorization_expired");
    short_lived.finish().await;

    let other = Harness::start(Behaviour {
        token_script: vec![oauth_error("invalid_grant")],
        ..Behaviour::default()
    })
    .await;
    assert_eq!(other.fail().await, "upstream");
    other.finish().await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_server_without_the_device_grant_is_unsupported() {
    let no_endpoint = Harness::start(Behaviour {
        metadata: json!({ "device_authorization_endpoint": null }),
        ..Behaviour::default()
    })
    .await;
    assert_eq!(no_endpoint.fail().await, "unsupported");
    assert!(no_endpoint
        .server
        .device_requests
        .lock()
        .unwrap()
        .is_empty());
    no_endpoint.finish().await;

    let no_grant_type = Harness::start(Behaviour {
        metadata: json!({ "grant_types_supported": ["authorization_code", "refresh_token"] }),
        ..Behaviour::default()
    })
    .await;
    assert_eq!(no_grant_type.fail().await, "unsupported");
    assert!(no_grant_type
        .server
        .device_requests
        .lock()
        .unwrap()
        .is_empty());
    no_grant_type.finish().await;
}

/// A link or code that fails validation ends the flow before either is
/// published and before the token endpoint is ever polled.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn an_unsafe_link_or_code_is_never_published() {
    for (field, value) in [
        ("verification_uri", json!("javascript:alert(1)")),
        (
            "verification_uri_complete",
            json!("https://user:pass@auth.example.org/link"),
        ),
        // Plain HTTP to a host that is not loopback, even from a loopback
        // homeserver.
        ("verification_uri", json!("http://auth.example.org/link")),
        ("user_code", json!("WDJB\u{1b}[2J")),
    ] {
        let mut behaviour = never_approved();
        behaviour.device_authorization[field] = value;
        let harness = Harness::start(behaviour).await;
        let flow = harness.create().await;

        let mut seen = Vec::new();
        let failed = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let state = harness.engine.get_browser(flow.flow_id).unwrap();
                seen.push(state.stage);
                assert!(
                    state.verification_uri.is_none() && state.user_code.is_none(),
                    "an invalid {field} must not be published"
                );
                if state.stage == MatrixOAuthBrowserStage::Failed {
                    return state;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("flow failed in time");
        assert_eq!(failed.error_code.as_deref(), Some("upstream"));
        assert!(!seen.contains(&MatrixOAuthBrowserStage::WaitingForAuthorization));
        assert_eq!(harness.server.token_requests.load(Ordering::SeqCst), 0);
        harness.finish().await;
    }
}

/// Tokens that cannot become the expected account are refused after issue,
/// and nothing is left behind for the identity.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_session_for_the_wrong_identity_or_without_refresh_is_refused() {
    let cases = [
        (
            "a different Matrix user",
            Behaviour {
                whoami_user: Some("@mallory:example.org".to_owned()),
                ..Behaviour::default()
            },
            "user_mismatch",
        ),
        (
            "a different device than the scope named",
            Behaviour {
                whoami_device: Some("SOMEOTHERDEVICE".to_owned()),
                ..Behaviour::default()
            },
            "upstream",
        ),
        (
            "no refresh token",
            Behaviour {
                approval: (
                    StatusCode::OK,
                    json!({ "access_token": ACCESS_TOKEN, "token_type": "Bearer" }),
                ),
                ..Behaviour::default()
            },
            "upstream",
        ),
    ];
    for (case, behaviour, expected) in cases {
        let harness = Harness::start(behaviour).await;
        assert_eq!(harness.fail().await, expected, "{case}");
        harness.wait_until_released().await;
        assert_eq!(harness.staging_dirs(), 0, "{case}: staged store removed");
        assert!(
            harness
                .store
                .list_accounts()
                .await
                .unwrap()
                .iter()
                .all(|account| account.user_id != harness.user_id),
            "{case}: no account may be activated"
        );
        harness.finish().await;
    }
}

/// A login that makes the identity active while the user is still approving
/// wins; the browser session is discarded rather than replacing a live device.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_concurrent_login_wins_over_a_browser_sign_in() {
    let harness = Harness::start(Behaviour {
        token_script: vec![pending(), pending(), pending()],
        ..Behaviour::default()
    })
    .await;
    let flow = harness.create().await;
    harness
        .wait_for(
            flow.flow_id,
            MatrixOAuthBrowserStage::WaitingForAuthorization,
        )
        .await;

    let winner = harness
        .store
        .upsert_account(&harness.user_id, &harness.server.base)
        .await
        .unwrap();
    assert_eq!(winner.state, AccountState::Active);

    let failed = harness
        .wait_for(flow.flow_id, MatrixOAuthBrowserStage::Failed)
        .await;
    assert_eq!(failed.error_code.as_deref(), Some("conflict"));
    harness.wait_until_released().await;
    assert_eq!(harness.staging_dirs(), 0);

    let account = harness
        .store
        .get_account(winner.account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.state, AccountState::Active);
    assert_ne!(
        account.device_id.as_deref(),
        Some(harness.server.requested_device_id().as_str()),
        "the live account must not be moved onto the discarded device"
    );
    harness.finish().await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn an_invalid_expected_user_id_is_refused_before_any_work() {
    let harness = Harness::start(Behaviour::default()).await;
    assert!(matches!(
        harness.engine.create_browser("not-a-matrix-id").await,
        Err(MatrixOAuthAcquireError::InvalidUserId)
    ));
    harness.finish().await;
}
