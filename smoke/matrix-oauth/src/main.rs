//! Black-box ADR 0097 smoke and interoperability harness.
//!
//! The harness talks to the shipped `axon` binary only over `/v1/`. A pinned
//! matrix-rust-sdk client plays the independent trusted or fresh Matrix device.
//! Protocol secrets are retained in memory solely so the Axon log can be
//! checked for accidental disclosure; the harness emits only stable phase
//! names and creates no failure artifact.

use std::{
    fs::OpenOptions,
    future::{Future, IntoFuture},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use matrix_sdk::{
    authentication::{
        matrix::MatrixSession,
        oauth::{
            qrcode::{GeneratedQrProgress, GrantLoginProgress, LoginProgress},
            registration::{ApplicationType, ClientMetadata, Localized, OAuthGrantType},
        },
        SessionTokens,
    },
    config::{RequestConfig, SyncSettings},
    ruma::{
        api::client::room::create_room::v3::Request as CreateRoomRequest,
        events::room::message::RoomMessageEventContent, serde::Raw, OwnedEventId, OwnedRoomId,
        OwnedUserId,
    },
    store::RoomLoadSettings,
    Client, SessionMeta,
};
use reqwest::{Method, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const FLOW_TIMEOUT: Duration = Duration::from_secs(120);
const START_TIMEOUT: Duration = Duration::from_secs(120);
// Axon's sync-engine tracker has its own 30-second graceful drain budget.
// Leave room for the surrounding HTTP and process teardown before escalating.
const AXON_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(45);
const BODY_LIMIT: usize = 1024 * 1024;
const FLOW_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone)]
struct Config {
    mode: String,
    homeserver: String,
    mas_base: Url,
    database_url: String,
    compatibility_token: String,
    matrix_password: String,
    user_id: OwnedUserId,
    trusted_device_id: String,
    run_dir: PathBuf,
    axon_bin: PathBuf,
    axon_port: u16,
}

impl Config {
    fn load() -> Result<Self> {
        let mode = std::env::args()
            .nth(1)
            .ok_or_else(|| anyhow!("missing lane name"))?;
        if !matches!(mode.as_str(), "api" | "acquire" | "grant" | "unsupported") {
            bail!("unknown lane name");
        }
        let user_id = OwnedUserId::try_from(required("MATRIX_OAUTH_USER_ID")?)
            .map_err(|_| anyhow!("invalid MATRIX_OAUTH_USER_ID"))?;
        Ok(Self {
            mode,
            homeserver: required("MATRIX_OAUTH_HOMESERVER")?,
            mas_base: Url::parse(&required("MATRIX_OAUTH_MAS_BASE")?)
                .context("parse MATRIX_OAUTH_MAS_BASE")?,
            database_url: required("MATRIX_OAUTH_DATABASE_URL")?,
            compatibility_token: required("MATRIX_OAUTH_COMPATIBILITY_TOKEN")?,
            matrix_password: required("MATRIX_OAUTH_MATRIX_PASSWORD")?,
            user_id,
            trusted_device_id: required("MATRIX_OAUTH_TRUSTED_DEVICE_ID")?,
            run_dir: PathBuf::from(required("MATRIX_OAUTH_HARNESS_RUN_DIR")?),
            axon_bin: PathBuf::from(required("AXON_SERVER_BIN")?),
            axon_port: required("MATRIX_OAUTH_AXON_PORT")?
                .parse()
                .map_err(|_| anyhow!("invalid MATRIX_OAUTH_AXON_PORT"))?,
        })
    }
}

fn required(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("missing required environment variable {name}"))
}

#[derive(Clone, Default)]
struct SecretTracker(Arc<Mutex<Vec<String>>>);

impl SecretTracker {
    fn remember(&self, value: impl Into<String>) {
        let value = value.into();
        if !value.is_empty() {
            self.0.lock().expect("secret tracker lock").push(value);
        }
    }

    fn assert_absent_from(&self, path: &Path) -> Result<()> {
        let log = std::fs::read_to_string(path).context("read Axon log for disclosure check")?;
        let lower = log.to_ascii_lowercase();
        for forbidden in [
            "qr_code_data",
            "check_code",
            "authorization_user_code",
            "verification_uri",
            "access_token",
            "refresh_token",
            "secrets_bundle",
            "recovery_key",
        ] {
            if lower.contains(forbidden) {
                bail!("Axon log contains secret-bearing field name {forbidden}");
            }
        }
        for secret in self.0.lock().expect("secret tracker lock").iter() {
            if secret.len() >= 6 && log.contains(secret) {
                bail!("Axon log contains runtime protocol material");
            }
        }
        Ok(())
    }
}

struct AxonProcess {
    config: Config,
    child: Option<Child>,
    log_path: PathBuf,
    store_key: String,
    bearer_token: String,
}

impl AxonProcess {
    fn prepare(config: Config, secrets: &SecretTracker) -> Result<Self> {
        std::fs::create_dir_all(&config.run_dir).context("create harness run directory")?;
        let log_path = config.run_dir.join("axon.log");
        let store_key = format!("matrix-oauth-smoke-{}", Uuid::new_v4());
        let bearer_token = issue_axon_token(&config, &store_key)?;
        secrets.remember(config.compatibility_token.clone());
        secrets.remember(config.matrix_password.clone());
        secrets.remember(store_key.clone());
        secrets.remember(bearer_token.clone());

        Ok(Self {
            config,
            child: None,
            log_path,
            store_key,
            bearer_token,
        })
    }

    fn api(&self) -> Result<Api> {
        Api::new(
            format!("http://127.0.0.1:{}", self.config.axon_port),
            self.bearer_token.clone(),
            self.config.user_id.to_string(),
        )
    }

    async fn spawn(&mut self) -> Result<()> {
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .context("open Axon log")?;
        let stderr = stdout.try_clone().context("clone Axon log handle")?;
        let mut command = Command::new(&self.config.axon_bin);
        apply_axon_env(&mut command, &self.config, &self.store_key);
        let child = command
            .current_dir(&self.config.run_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .context("spawn axon-server")?;
        let port = self.config.axon_port;
        self.child = Some(child);
        wait_for_health(
            port,
            self.child
                .as_mut()
                .expect("spawned Axon child remains owned"),
        )
        .await
    }

    async fn stop(&mut self) -> Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        if child.try_wait().context("inspect axon-server")?.is_some() {
            return Ok(());
        }
        #[cfg(unix)]
        unsafe {
            if libc::kill(child.id() as i32, libc::SIGTERM) != 0 {
                let signal_error = std::io::Error::last_os_error();
                if child
                    .try_wait()
                    .context("reinspect axon-server after signal failure")?
                    .is_some()
                {
                    return Ok(());
                }
                return Err(signal_error).context("signal axon-server");
            }
        }
        let deadline = tokio::time::Instant::now() + AXON_SHUTDOWN_TIMEOUT;
        loop {
            if child.try_wait().context("wait for axon-server")?.is_some() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                child.kill().context("kill unresponsive axon-server")?;
                let _ = child.wait();
                bail!("axon-server did not stop within its shutdown budget");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for AxonProcess {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn issue_axon_token(config: &Config, store_key: &str) -> Result<String> {
    let mut command = Command::new(&config.axon_bin);
    apply_axon_env(&mut command, config, store_key);
    let output = command
        .current_dir(&config.run_dir)
        .args(["token", "issue", "--label", "matrix-oauth-smoke"])
        .output()
        .context("run axon token issue")?;
    if !output.status.success() {
        bail!("axon token issue failed");
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .last()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("axon token issue returned no token"))
}

fn apply_axon_env(command: &mut Command, config: &Config, store_key: &str) {
    command
        // The harness needs these values for its independent SDK/MAS actors,
        // but the Axon subprocess must never inherit them.
        .env_remove("MATRIX_OAUTH_COMPATIBILITY_TOKEN")
        .env_remove("MATRIX_OAUTH_MATRIX_PASSWORD")
        .env("DATABASE_URL", &config.database_url)
        .env("AXON_SERVER__PORT", config.axon_port.to_string())
        .env("AXON_SYNC__STORE_KEY", store_key)
        .env("AXON_SYNC__DATA_DIR", config.run_dir.join("sync"))
        .env("AXON_SEARCH__INDEX_PATH", config.run_dir.join("search"))
        .env("AXON_MEDIA__CACHE_DIR", config.run_dir.join("media"))
        .env("AXON_MEDIA__UPLOADS_DIR", config.run_dir.join("uploads"))
        .env("AXON_SYNC__MATRIX_OAUTH__REQUEST_TIMEOUT_SECS", "15")
        .env(
            "RUST_LOG",
            "info,sqlx::query=error,matrix_sdk_crypto=error,matrix_sdk::encryption::backups=error,axon_sync=debug",
        );
}

fn ensure_axon_is_running(child: &mut Child) -> Result<()> {
    if let Some(status) = child
        .try_wait()
        .context("inspect axon-server during startup")?
    {
        bail!("axon-server exited before becoming healthy with {status}");
    }
    Ok(())
}

async fn wait_for_health(port: u16, child: &mut Child) -> Result<()> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("build health client")?;
    let url = format!("http://127.0.0.1:{port}/healthz");
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        ensure_axon_is_running(child)?;
        if http
            .get(&url)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return Ok(());
        }
        ensure_axon_is_running(child)?;
        if tokio::time::Instant::now() >= deadline {
            bail!("axon-server did not become healthy");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[derive(Clone)]
struct Api {
    http: reqwest::Client,
    base: String,
    token: String,
    expected_user_id: String,
}

impl Api {
    fn new(base: String, token: String, expected_user_id: String) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .context("build Axon API client")?,
            base,
            token,
            expected_user_id,
        })
    }

    async fn raw(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        authenticated: bool,
    ) -> Result<(StatusCode, String)> {
        let mut request = self.http.request(method, format!("{}{}", self.base, path));
        if authenticated {
            request = request.bearer_auth(&self.token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.context("Axon API request failed")?;
        let status = response.status();
        let bytes = bounded_body(
            response,
            "read Axon API response failed",
            "Axon API response exceeded the harness limit",
        )
        .await?;
        let text = String::from_utf8(bytes).context("Axon API response was not UTF-8")?;
        Ok((status, text))
    }

    async fn envelope<T: DeserializeOwned>(
        &self,
        operation: &'static str,
        method: Method,
        path: &str,
        body: Option<Value>,
        expected: StatusCode,
    ) -> Result<T> {
        let (status, text) = self.raw(method, path, body, true).await?;
        if status != expected {
            bail!("Axon API {operation} returned HTTP {status}; expected {expected}");
        }
        let envelope: Envelope<T> = serde_json::from_str(&text)
            .map_err(|_| anyhow!("Axon API {operation} returned an invalid success envelope"))?;
        Ok(envelope.data)
    }

    async fn create_acquire(&self) -> Result<AcquireFlow> {
        self.envelope(
            "acquisition creation",
            Method::POST,
            "/v1/accounts/login/qr",
            Some(json!({ "expected_user_id": self.expected_user_id.as_str(), "presentation": "scan" })),
            StatusCode::CREATED,
        )
        .await
    }

    async fn create_acquire_after_cancellation(&self) -> Result<AcquireFlow> {
        let deadline = tokio::time::Instant::now() + START_TIMEOUT;
        loop {
            let (status, text) = self
                .raw(
                    Method::POST,
                    "/v1/accounts/login/qr",
                    Some(json!({ "expected_user_id": self.expected_user_id.as_str(), "presentation": "scan" })),
                    true,
                )
                .await?;
            if status == StatusCode::CREATED {
                let envelope: Envelope<AcquireFlow> = serde_json::from_str(&text).map_err(|_| {
                    anyhow!(
                        "Axon API acquisition creation after cancellation returned an invalid success envelope"
                    )
                })?;
                return Ok(envelope.data);
            }
            if status != StatusCode::CONFLICT {
                bail!(
                    "Axon API acquisition creation after cancellation returned HTTP {status}; expected 201 Created or transient 409 Conflict"
                );
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("cancelled acquisition did not release its identity reservation in time");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn acquire(&self, flow_id: Uuid) -> Result<AcquireFlow> {
        self.envelope(
            "acquisition lookup",
            Method::GET,
            &format!("/v1/accounts/login/qr/{flow_id}"),
            None,
            StatusCode::OK,
        )
        .await
    }

    async fn acquire_scan(&self, flow_id: Uuid, qr_code_data: &str) -> Result<()> {
        let _: AcquireFlow = self
            .envelope(
                "acquisition scan submission",
                Method::POST,
                &format!("/v1/accounts/login/qr/{flow_id}/scan"),
                Some(json!({ "qr_code_data": qr_code_data })),
                StatusCode::OK,
            )
            .await?;
        Ok(())
    }

    async fn delete_acquire(&self, flow_id: Uuid) -> Result<()> {
        let (status, _) = self
            .raw(
                Method::DELETE,
                &format!("/v1/accounts/login/qr/{flow_id}"),
                None,
                true,
            )
            .await?;
        if status != StatusCode::NO_CONTENT {
            bail!(
                "Axon API acquisition cancellation returned HTTP {status}; expected 204 No Content"
            );
        }
        Ok(())
    }

    async fn create_grant(&self, account_id: Uuid, presentation: &str) -> Result<GrantFlow> {
        self.envelope(
            "grant creation",
            Method::POST,
            &format!("/v1/accounts/{account_id}/login-grants/qr"),
            Some(json!({ "presentation": presentation })),
            StatusCode::CREATED,
        )
        .await
    }

    async fn grant(&self, account_id: Uuid, flow_id: Uuid) -> Result<GrantFlow> {
        self.envelope(
            "grant lookup",
            Method::GET,
            &format!("/v1/accounts/{account_id}/login-grants/qr/{flow_id}"),
            None,
            StatusCode::OK,
        )
        .await
    }

    async fn grant_scan(&self, account_id: Uuid, flow_id: Uuid, qr_code_data: &str) -> Result<()> {
        let _: GrantFlow = self
            .envelope(
                "grant scan submission",
                Method::POST,
                &format!("/v1/accounts/{account_id}/login-grants/qr/{flow_id}/scan"),
                Some(json!({ "qr_code_data": qr_code_data })),
                StatusCode::OK,
            )
            .await?;
        Ok(())
    }

    async fn delete_grant(&self, account_id: Uuid, flow_id: Uuid) -> Result<()> {
        let (status, _) = self
            .raw(
                Method::DELETE,
                &format!("/v1/accounts/{account_id}/login-grants/qr/{flow_id}"),
                None,
                true,
            )
            .await?;
        if status != StatusCode::NO_CONTENT {
            bail!("Axon API grant cancellation returned HTTP {status}; expected 204 No Content");
        }
        Ok(())
    }

    async fn accounts(&self) -> Result<Vec<Account>> {
        self.envelope(
            "account listing",
            Method::GET,
            "/v1/accounts",
            None,
            StatusCode::OK,
        )
        .await
    }

    async fn timeline(&self, account_id: Uuid, room_id: &str) -> Result<Timeline> {
        let encoded: String = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect();
        self.envelope(
            "timeline lookup",
            Method::GET,
            &format!("/v1/accounts/{account_id}/rooms/{encoded}/timeline?limit=20"),
            None,
            StatusCode::OK,
        )
        .await
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
}

#[derive(Clone, Deserialize)]
struct AcquireFlow {
    flow_id: Uuid,
    stage: String,
    account_id: Option<Uuid>,
    qr_code_data: Option<String>,
    check_code: Option<String>,
    authorization_user_code: Option<String>,
    verification_uri: Option<String>,
    error_code: Option<String>,
}

#[derive(Clone, Deserialize)]
struct GrantFlow {
    flow_id: Uuid,
    account_id: Uuid,
    stage: String,
    qr_code_data: Option<String>,
    check_code: Option<String>,
    verification_uri: Option<String>,
    error_code: Option<String>,
}

trait FlowState {
    fn stage(&self) -> &str;
}

impl FlowState for AcquireFlow {
    fn stage(&self) -> &str {
        &self.stage
    }
}

impl FlowState for GrantFlow {
    fn stage(&self) -> &str {
        &self.stage
    }
}

#[derive(Deserialize)]
struct Account {
    account_id: Uuid,
    user_id: String,
    state: String,
    verified: bool,
}

#[derive(Deserialize)]
struct Timeline {
    events: Vec<TimelineEvent>,
}

#[derive(Deserialize)]
struct TimelineEvent {
    body: Option<String>,
}

struct SeededPeer {
    client: Client,
    room_id: OwnedRoomId,
    history_event_id: OwnedEventId,
    history_marker: String,
}

async fn seed_trusted_peer(config: &Config, secrets: &SecretTracker) -> Result<SeededPeer> {
    let store = config.run_dir.join("trusted-sdk");
    tokio::fs::create_dir_all(&store)
        .await
        .context("create trusted SDK store")?;
    let client = Client::builder()
        .homeserver_url(&config.homeserver)
        .request_config(RequestConfig::new().timeout(HTTP_TIMEOUT))
        .sqlite_store(&store, Some("trusted-sdk-store-key"))
        .build()
        .await
        .map_err(|_| anyhow!("build trusted SDK client failed"))?;
    client
        .matrix_auth()
        .restore_session(
            MatrixSession {
                meta: SessionMeta {
                    user_id: config.user_id.clone(),
                    device_id: config.trusted_device_id.as_str().into(),
                },
                tokens: SessionTokens {
                    access_token: config.compatibility_token.clone(),
                    refresh_token: None,
                },
            },
            RoomLoadSettings::default(),
        )
        .await
        .map_err(|_| anyhow!("restore trusted SDK session failed"))?;
    client
        .sync_once(SyncSettings::default())
        .await
        .map_err(|_| anyhow!("initial trusted-device sync failed"))?;
    client
        .encryption()
        .bootstrap_cross_signing(None)
        .await
        .map_err(|_| anyhow!("cross-signing bootstrap failed"))?;
    client
        .sync_once(SyncSettings::default())
        .await
        .map_err(|_| anyhow!("post-bootstrap sync failed"))?;
    let trusted = client
        .encryption()
        .get_own_device()
        .await
        .map_err(|_| anyhow!("trusted-device lookup failed"))?
        .is_some_and(|device| device.is_cross_signed_by_owner());
    if !trusted {
        bail!("seed device is not cross-signed");
    }

    let mut request = CreateRoomRequest::new();
    request.name = Some("Matrix OAuth interoperability".to_owned());
    let room = client
        .create_room(request)
        .await
        .map_err(|_| anyhow!("encrypted-room creation failed"))?;
    room.enable_encryption()
        .await
        .map_err(|_| anyhow!("room encryption enable failed"))?;
    client
        .sync_once(SyncSettings::default())
        .await
        .map_err(|_| anyhow!("encrypted-room sync failed"))?;
    let history_marker = format!("matrix-oauth-history-{}", Uuid::new_v4());
    let history_event = room
        .send(RoomMessageEventContent::text_plain(&history_marker))
        .await
        .map_err(|_| anyhow!("encrypted history send failed"))?;
    let recovery_key = tokio::time::timeout(
        FLOW_TIMEOUT,
        client
            .encryption()
            .recovery()
            .enable()
            .wait_for_backups_to_upload(),
    )
    .await
    .map_err(|_| anyhow!("secure-backup upload timed out"))?
    .map_err(|_| anyhow!("secure-backup enable failed"))?;
    if recovery_key.is_empty() {
        bail!("secure-backup enable returned no recovery key");
    }
    secrets.remember(recovery_key);
    Ok(SeededPeer {
        client,
        room_id: room.room_id().to_owned(),
        history_event_id: history_event.response.event_id,
        history_marker,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Approval {
    Consent,
    Reject,
}

struct MasApprover {
    base: Url,
    username: String,
    password: String,
}

impl MasApprover {
    async fn act(&self, verification_uri: &str, action: Approval) -> Result<()> {
        let uri =
            Url::parse(verification_uri).map_err(|_| anyhow!("invalid MAS verification URI"))?;
        if uri.scheme() != self.base.scheme()
            || uri.host_str() != self.base.host_str()
            || uri.port_or_known_default() != self.base.port_or_known_default()
        {
            bail!("verification URI did not target the test authorization server");
        }
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::limited(8))
            .timeout(HTTP_TIMEOUT)
            .build()
            .context("build MAS approval client")?;
        let response = client
            .get(uri)
            .send()
            .await
            .map_err(|_| anyhow!("open MAS approval failed"))?;
        let (mut page_url, mut page) = bounded_html(response).await?;
        if page.contains("name=\"username\"") {
            let csrf = extract_csrf(&page)?;
            let response = client
                .post(page_url.clone())
                .form(&[
                    ("csrf", csrf.as_str()),
                    ("username", self.username.as_str()),
                    ("password", self.password.as_str()),
                ])
                .send()
                .await
                .map_err(|_| anyhow!("MAS password login failed"))?;
            (page_url, page) = bounded_html(response).await?;
        }
        if !page.contains("name=\"action\"") {
            bail!("MAS did not render the device approval form");
        }
        let csrf = extract_csrf(&page)?;
        let mut fields = vec![
            ("csrf", csrf.as_str()),
            (
                "action",
                if action == Approval::Consent {
                    "consent"
                } else {
                    "reject"
                },
            ),
        ];
        if action == Approval::Consent {
            if !page.contains("name=\"confirm_device\"") {
                bail!("MAS approval form omitted explicit device confirmation");
            }
            fields.push(("confirm_device", "on"));
        }
        let response = client
            .post(page_url)
            .form(&fields)
            .send()
            .await
            .map_err(|_| anyhow!("submit MAS approval failed"))?;
        let (_, page) = bounded_html(response).await?;
        assert_mas_decision(&page, action)?;
        Ok(())
    }
}

fn assert_mas_decision(page: &str, action: Approval) -> Result<()> {
    let outcome_element = match action {
        Approval::Consent => "<div class=\"icon success\">",
        Approval::Reject => "<div class=\"icon invalid\">",
    };
    if !page.contains(outcome_element) || page.contains("name=\"action\"") {
        bail!("MAS did not confirm the requested approval decision");
    }
    Ok(())
}

async fn bounded_html(response: reqwest::Response) -> Result<(Url, String)> {
    if !response.status().is_success() {
        bail!("MAS returned an unexpected status");
    }
    let url = response.url().clone();
    let bytes = bounded_body(
        response,
        "read MAS page failed",
        "MAS page exceeded the harness limit",
    )
    .await?;
    let body = String::from_utf8(bytes).map_err(|_| anyhow!("MAS page was not UTF-8"))?;
    Ok((url, body))
}

async fn bounded_body(
    mut response: reqwest::Response,
    read_error: &'static str,
    limit_error: &'static str,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|size| size > BODY_LIMIT as u64)
    {
        bail!("{limit_error}");
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| anyhow!(read_error))? {
        if chunk.len() > BODY_LIMIT.saturating_sub(body.len()) {
            bail!("{limit_error}");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn extract_csrf(page: &str) -> Result<String> {
    page.split("name=\"csrf\" value=\"")
        .nth(1)
        .and_then(|tail| tail.split('"').next())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("MAS form omitted its CSRF field"))
}

fn client_metadata() -> Raw<ClientMetadata> {
    let client_uri = Localized::new(
        Url::parse("https://github.com/matrix-axon/matrix-axon")
            .expect("static client URI is valid"),
        [],
    );
    Raw::new(&ClientMetadata {
        client_name: Some(Localized::new("Axon interoperability SDK".to_owned(), [])),
        ..ClientMetadata::new(
            ApplicationType::Native,
            vec![OAuthGrantType::DeviceCode],
            client_uri,
        )
    })
    .expect("static client metadata serializes")
}

async fn drive_peer_grant(
    peer: &Client,
    api: &Api,
    flow_id: Uuid,
    approver: &MasApprover,
    secrets: &SecretTracker,
    approval: Approval,
) -> Result<()> {
    let oauth = peer.oauth();
    let grant = oauth.grant_login_with_qr_code().generate();
    let mut progress = Box::pin(grant.subscribe_to_progress());
    let mut future = grant.into_future();
    let mut progress_ended = false;
    let deadline = tokio::time::Instant::now() + FLOW_TIMEOUT;
    loop {
        tokio::select! {
            result = &mut future => {
                return match (approval, result.is_ok()) {
                    (Approval::Consent, true) | (Approval::Reject, false) => Ok(()),
                    (Approval::Consent, false) => Err(anyhow!("trusted SDK grant failed")),
                    (Approval::Reject, true) => Err(anyhow!("rejected authorization unexpectedly completed")),
                };
            }
            update = progress.next(), if !progress_ended => match update {
                Some(GrantLoginProgress::Starting | GrantLoginProgress::SyncingSecrets) => {}
                Some(GrantLoginProgress::EstablishingSecureChannel(GeneratedQrProgress::QrReady(qr))) => {
                    let encoded = qr.to_base64();
                    secrets.remember(encoded.clone());
                    api.acquire_scan(flow_id, &encoded).await?;
                }
                Some(GrantLoginProgress::EstablishingSecureChannel(GeneratedQrProgress::QrScanned(sender))) => {
                    let state = wait_acquire_stage(api, flow_id, "check_code_to_display", FLOW_TIMEOUT).await?;
                    let code = parse_check_code(state.check_code.as_deref())?;
                    tokio::time::timeout(HTTP_TIMEOUT, sender.send(code))
                        .await
                        .map_err(|_| anyhow!("trusted-device check-code submission timed out"))?
                        .map_err(|_| anyhow!("submit trusted-device check code failed"))?;
                }
                Some(GrantLoginProgress::WaitingForAuth { verification_uri }) => {
                    let state = wait_acquire_stage(api, flow_id, "waiting_for_authorization", FLOW_TIMEOUT).await?;
                    let user_code = state.authorization_user_code.ok_or_else(|| anyhow!("acquire flow omitted its authorization user code"))?;
                    secrets.remember(user_code);
                    let uri = verification_uri.to_string();
                    secrets.remember(uri.clone());
                    assert_waits_for_approval(|| api.acquire(flow_id), "acquire").await?;
                    approver.act(&uri, approval).await?;
                }
                Some(GrantLoginProgress::Done) | None => progress_ended = true,
            },
            _ = tokio::time::sleep_until(deadline) => bail!("trusted SDK grant timed out"),
        }
    }
}

async fn drive_fresh_login(
    fresh: &Client,
    api: &Api,
    account_id: Uuid,
    flow_id: Uuid,
    approver: &MasApprover,
    secrets: &SecretTracker,
) -> Result<()> {
    let registration = client_metadata().into();
    let oauth = fresh.oauth();
    let login = oauth.login_with_qr_code(Some(&registration)).generate();
    let mut progress = Box::pin(login.subscribe_to_progress());
    let mut future = login.into_future();
    let mut progress_ended = false;
    let deadline = tokio::time::Instant::now() + FLOW_TIMEOUT;
    loop {
        tokio::select! {
            result = &mut future => {
                return result.map_err(|_| anyhow!("fresh SDK QR login failed"));
            }
            update = progress.next(), if !progress_ended => match update {
                Some(LoginProgress::Starting | LoginProgress::SyncingSecrets) => {}
                Some(LoginProgress::EstablishingSecureChannel(GeneratedQrProgress::QrReady(qr))) => {
                    let encoded = qr.to_base64();
                    secrets.remember(encoded.clone());
                    api.grant_scan(account_id, flow_id, &encoded).await?;
                }
                Some(LoginProgress::EstablishingSecureChannel(GeneratedQrProgress::QrScanned(sender))) => {
                    let state = wait_grant_stage(api, account_id, flow_id, "check_code_to_display", FLOW_TIMEOUT).await?;
                    let code = parse_check_code(state.check_code.as_deref())?;
                    tokio::time::timeout(HTTP_TIMEOUT, sender.send(code))
                        .await
                        .map_err(|_| anyhow!("fresh-device check-code submission timed out"))?
                        .map_err(|_| anyhow!("submit fresh-device check code failed"))?;
                }
                Some(LoginProgress::WaitingForToken { user_code }) => {
                    secrets.remember(user_code);
                    let state = wait_grant_stage(api, account_id, flow_id, "waiting_for_authorization", FLOW_TIMEOUT).await?;
                    let uri = state.verification_uri.ok_or_else(|| anyhow!("grant flow omitted its authorization URI"))?;
                    secrets.remember(uri.clone());
                    assert_waits_for_approval(|| api.grant(account_id, flow_id), "grant").await?;
                    approver.act(&uri, Approval::Consent).await?;
                }
                Some(LoginProgress::Done) | None => progress_ended = true,
            },
            _ = tokio::time::sleep_until(deadline) => bail!("fresh SDK QR login timed out"),
        }
    }
}

fn parse_check_code(value: Option<&str>) -> Result<u8> {
    value
        .ok_or_else(|| anyhow!("flow omitted its check code"))?
        .parse()
        .map_err(|_| anyhow!("flow returned an invalid check code"))
}

async fn assert_waits_for_approval<T, F, Fut>(mut fetch: F, flow: &'static str) -> Result<()>
where
    T: FlowState,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    tokio::time::sleep(Duration::from_secs(2)).await;
    if fetch().await?.stage() != "waiting_for_authorization" {
        bail!("{flow} flow completed without explicit authorization-server approval");
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum FlowWait<'a> {
    Stage(&'a str),
    Terminal,
}

fn is_terminal(stage: &str) -> bool {
    matches!(stage, "failed" | "cancelled" | "done")
}

async fn wait_for_flow<T, F, Fut>(
    mut fetch: F,
    target: FlowWait<'_>,
    timeout: Duration,
    flow: &'static str,
) -> Result<T>
where
    T: FlowState,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let state = fetch().await?;
        let terminal = is_terminal(state.stage());
        match target {
            FlowWait::Stage(wanted) if state.stage() == wanted => return Ok(state),
            FlowWait::Terminal if terminal => return Ok(state),
            FlowWait::Stage(_) if terminal => {
                bail!("{flow} flow reached an unexpected terminal stage");
            }
            FlowWait::Stage(_) | FlowWait::Terminal => {}
        }
        if tokio::time::Instant::now() >= deadline {
            let target_name = match target {
                FlowWait::Stage(_) => "stage",
                FlowWait::Terminal => "terminal",
            };
            bail!("{flow} {target_name} wait timed out");
        }
        tokio::time::sleep(FLOW_POLL_INTERVAL).await;
    }
}

async fn wait_acquire_stage(
    api: &Api,
    flow_id: Uuid,
    wanted: &str,
    timeout: Duration,
) -> Result<AcquireFlow> {
    wait_for_flow(
        || api.acquire(flow_id),
        FlowWait::Stage(wanted),
        timeout,
        "acquire",
    )
    .await
}

async fn wait_grant_stage(
    api: &Api,
    account_id: Uuid,
    flow_id: Uuid,
    wanted: &str,
    timeout: Duration,
) -> Result<GrantFlow> {
    wait_for_flow(
        || api.grant(account_id, flow_id),
        FlowWait::Stage(wanted),
        timeout,
        "grant",
    )
    .await
}

async fn wait_acquire_terminal(api: &Api, flow_id: Uuid) -> Result<AcquireFlow> {
    wait_for_flow(
        || api.acquire(flow_id),
        FlowWait::Terminal,
        FLOW_TIMEOUT,
        "acquire",
    )
    .await
}

async fn wait_grant_terminal(
    api: &Api,
    account_id: Uuid,
    flow_id: Uuid,
    timeout: Duration,
) -> Result<GrantFlow> {
    wait_for_flow(
        || api.grant(account_id, flow_id),
        FlowWait::Terminal,
        timeout,
        "grant",
    )
    .await
}

async fn acquire_axon(
    api: &Api,
    peer: &SeededPeer,
    approver: &MasApprover,
    secrets: &SecretTracker,
) -> Result<Uuid> {
    let flow = api.create_acquire().await?;
    drive_peer_grant(
        &peer.client,
        api,
        flow.flow_id,
        approver,
        secrets,
        Approval::Consent,
    )
    .await?;
    let terminal = wait_acquire_terminal(api, flow.flow_id).await?;
    if terminal.stage != "done" || terminal.error_code.is_some() {
        bail!(
            "Axon acquisition ended in {} ({})",
            terminal.stage,
            terminal.error_code.as_deref().unwrap_or("no_error_code")
        );
    }
    if terminal.qr_code_data.is_some()
        || terminal.check_code.is_some()
        || terminal.authorization_user_code.is_some()
        || terminal.verification_uri.is_some()
    {
        bail!("acquire terminal state retained stage-specific protocol material");
    }
    terminal
        .account_id
        .ok_or_else(|| anyhow!("completed acquisition omitted account id"))
}

async fn wait_account_ready(api: &Api, account_id: Uuid) -> Result<()> {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        if api.accounts().await?.iter().any(|account| {
            account.account_id == account_id
                && account.user_id == api.expected_user_id.as_str()
                && account.state == "active"
                && account.verified
        }) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("acquired account did not become active and verified");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn wait_axon_decrypts(api: &Api, account_id: Uuid, peer: &SeededPeer) -> Result<()> {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        if api
            .timeline(account_id, peer.room_id.as_str())
            .await
            .is_ok_and(|timeline| {
                timeline
                    .events
                    .iter()
                    .any(|event| event.body.as_deref() == Some(peer.history_marker.as_str()))
            })
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("Axon did not decrypt the pre-login encrypted history");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn fresh_client(config: &Config) -> Result<Client> {
    let store = config.run_dir.join(format!("fresh-sdk-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&store)
        .await
        .context("create fresh SDK store")?;
    Client::builder()
        .homeserver_url(&config.homeserver)
        .handle_refresh_tokens()
        .request_config(RequestConfig::new().timeout(HTTP_TIMEOUT))
        .sqlite_store(&store, Some("fresh-sdk-store-key"))
        .build()
        .await
        .map_err(|_| anyhow!("build fresh SDK client failed"))
}

async fn assert_sdk_decrypts_history(client: &Client, peer: &SeededPeer) -> Result<()> {
    let room_deadline = tokio::time::Instant::now() + START_TIMEOUT;
    let room = loop {
        if let Some(room) = client.get_room(&peer.room_id) {
            break room;
        }
        client
            .sync_once(SyncSettings::default())
            .await
            .map_err(|_| anyhow!("fresh-device history sync failed"))?;
        if tokio::time::Instant::now() >= room_deadline {
            bail!("fresh SDK device did not receive the encrypted room");
        }
    };

    let backup_deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        if client.encryption().backups().are_enabled().await {
            break;
        }
        if tokio::time::Instant::now() >= backup_deadline {
            bail!("fresh SDK device did not receive the backup secret");
        }
        client
            .sync_once(SyncSettings::default())
            .await
            .map_err(|_| anyhow!("fresh-device backup-state sync failed"))?;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    client
        .encryption()
        .backups()
        .download_room_keys_for_room(&peer.room_id)
        .await
        .map_err(|_| anyhow!("fresh-device backup download failed"))?;
    let decryption_deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        let event = room
            .event(&peer.history_event_id, None)
            .await
            .map_err(|_| anyhow!("fresh-device event fetch failed"))?;
        if event
            .raw()
            .get_field::<Value>("content")
            .ok()
            .flatten()
            .and_then(|content| {
                content
                    .get("body")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .as_deref()
            == Some(peer.history_marker.as_str())
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= decryption_deadline {
            bail!("fresh SDK device did not decrypt transferred history");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn run_acquire_lane(
    process: &mut AxonProcess,
    peer: &SeededPeer,
    approver: &MasApprover,
    secrets: &SecretTracker,
) -> Result<()> {
    eprintln!("matrix-oauth(acquire): acquiring and cross-signing Axon");
    let api = process.api()?;
    let account_id = acquire_axon(&api, peer, approver, secrets).await?;
    wait_account_ready(&api, account_id).await?;
    wait_axon_decrypts(&api, account_id, peer).await?;

    eprintln!("matrix-oauth(acquire): restarting after OAuth access-token expiry");
    process.stop().await?;
    tokio::time::sleep(Duration::from_secs(65)).await;
    process.spawn().await?;
    let api = process.api()?;
    wait_account_ready(&api, account_id).await?;
    wait_axon_decrypts(&api, account_id, peer).await?;
    Ok(())
}

async fn run_grant_lane(
    process: &AxonProcess,
    peer: &SeededPeer,
    approver: &MasApprover,
    secrets: &SecretTracker,
) -> Result<()> {
    eprintln!("matrix-oauth(grant): establishing a trusted Axon account");
    let api = process.api()?;
    let account_id = acquire_axon(&api, peer, approver, secrets).await?;
    wait_account_ready(&api, account_id).await?;

    eprintln!("matrix-oauth(grant): authorizing a fresh SDK device");
    let fresh = fresh_client(&process.config).await?;
    let flow = api.create_grant(account_id, "scan").await?;
    drive_fresh_login(&fresh, &api, account_id, flow.flow_id, approver, secrets).await?;
    let terminal = wait_grant_terminal(&api, account_id, flow.flow_id, FLOW_TIMEOUT).await?;
    if terminal.stage != "done" || terminal.error_code.is_some() {
        bail!(
            "Axon grant ended in {} ({})",
            terminal.stage,
            terminal.error_code.as_deref().unwrap_or("no_error_code")
        );
    }
    if terminal.account_id != account_id
        || terminal.qr_code_data.is_some()
        || terminal.check_code.is_some()
        || terminal.verification_uri.is_some()
    {
        bail!("grant terminal state retained stage-specific protocol material");
    }
    let trusted = fresh
        .encryption()
        .get_own_device()
        .await
        .map_err(|_| anyhow!("fresh-device trust lookup failed"))?
        .is_some_and(|device| device.is_cross_signed_by_owner());
    if !trusted {
        bail!("fresh SDK device is not cross-signed");
    }
    assert_sdk_decrypts_history(&fresh, peer).await
}

async fn run_api_lane(
    process: &AxonProcess,
    peer: &SeededPeer,
    approver: &MasApprover,
    secrets: &SecretTracker,
) -> Result<()> {
    eprintln!("matrix-oauth(api): checking authenticated acquisition boundaries");
    let api = process.api()?;
    let (status, _) = api
        .raw(
            Method::POST,
            "/v1/accounts/login/qr",
            Some(json!({ "expected_user_id": api.expected_user_id.as_str(), "presentation": "scan" })),
            false,
        )
        .await?;
    if status != StatusCode::UNAUTHORIZED {
        bail!("unauthenticated acquire creation was not rejected");
    }
    let (status, _) = api
        .raw(
            Method::POST,
            "/v1/accounts/login/qr",
            Some(json!({ "expected_user_id": "not-a-matrix-id", "presentation": "scan" })),
            true,
        )
        .await?;
    if status != StatusCode::BAD_REQUEST {
        bail!("invalid acquisition identity was not rejected");
    }

    let cancelled = api.create_acquire().await?;
    api.delete_acquire(cancelled.flow_id).await?;
    api.delete_acquire(cancelled.flow_id).await?;
    if wait_acquire_terminal(&api, cancelled.flow_id).await?.stage != "cancelled" {
        bail!("acquisition cancellation was not replayable");
    }

    eprintln!("matrix-oauth(api): checking authorization rejection");
    let rejected = api.create_acquire_after_cancellation().await?;
    drive_peer_grant(
        &peer.client,
        &api,
        rejected.flow_id,
        approver,
        secrets,
        Approval::Reject,
    )
    .await?;
    let rejected = wait_acquire_terminal(&api, rejected.flow_id).await?;
    if rejected.stage != "failed" || rejected.error_code.is_none() {
        bail!("authorization rejection did not produce a stable failed flow");
    }

    eprintln!("matrix-oauth(api): checking account-scoped grant boundaries");
    let account_id = acquire_axon(&api, peer, approver, secrets).await?;
    wait_account_ready(&api, account_id).await?;
    let grant = api.create_grant(account_id, "scan").await?;
    let wrong_account = Uuid::new_v4();
    let (status, _) = api
        .raw(
            Method::GET,
            &format!(
                "/v1/accounts/{wrong_account}/login-grants/qr/{}",
                grant.flow_id
            ),
            None,
            true,
        )
        .await?;
    if status != StatusCode::NOT_FOUND
        || api.grant(account_id, grant.flow_id).await?.flow_id != grant.flow_id
    {
        bail!("grant flow was not isolated to its owning account");
    }
    let malformed = format!("not-a-qr-{}", Uuid::new_v4());
    secrets.remember(malformed.clone());
    let (status, body) = api
        .raw(
            Method::POST,
            &format!(
                "/v1/accounts/{account_id}/login-grants/qr/{}/scan",
                grant.flow_id
            ),
            Some(json!({ "qr_code_data": malformed })),
            true,
        )
        .await?;
    if status != StatusCode::BAD_REQUEST || body.contains("not-a-qr-") {
        bail!("malformed QR diagnostics were not stable and secret-safe");
    }
    api.delete_grant(account_id, grant.flow_id).await?;
    api.delete_grant(account_id, grant.flow_id).await?;
    if wait_grant_terminal(&api, account_id, grant.flow_id, FLOW_TIMEOUT)
        .await?
        .stage
        != "cancelled"
    {
        bail!("grant cancellation was not replayable");
    }

    eprintln!("matrix-oauth(api): checking bounded rendezvous expiry");
    let timeout_flow = api.create_grant(account_id, "display").await?;
    let timeout_flow = wait_grant_terminal(
        &api,
        account_id,
        timeout_flow.flow_id,
        Duration::from_secs(90),
    )
    .await?;
    if timeout_flow.stage != "failed"
        || !matches!(
            timeout_flow.error_code.as_deref(),
            Some("rendezvous_expired" | "timeout")
        )
    {
        bail!("unattended grant did not fail with a bounded timeout classification");
    }
    Ok(())
}

async fn run_unsupported_lane(
    process: &AxonProcess,
    peer: &SeededPeer,
    approver: &MasApprover,
    secrets: &SecretTracker,
) -> Result<()> {
    eprintln!("matrix-oauth(unsupported): checking capability-gated acquisition");
    let api = process.api()?;
    let flow = api.create_acquire().await?;
    let _ = drive_peer_grant(
        &peer.client,
        &api,
        flow.flow_id,
        approver,
        secrets,
        Approval::Consent,
    )
    .await;
    let terminal = wait_acquire_terminal(&api, flow.flow_id).await?;
    if terminal.stage != "failed" || terminal.error_code.as_deref() != Some("unsupported") {
        bail!("missing device-authorization capability was not reported as unsupported");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run_until_shutdown().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // Every lower boundary maps its source to a stable harness error;
            // never print an SDK, OAuth, or HTTP source that may embed secrets.
            eprintln!("matrix-oauth: failed: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run_until_shutdown() -> Result<()> {
    let config = Config::load()?;
    let mode = config.mode.clone();
    let secrets = SecretTracker::default();
    let mut process = AxonProcess::prepare(config.clone(), &secrets)?;

    let lane_result = tokio::select! {
        // Keep startup cancellable while retaining the process outside this
        // future so the common stop, reap, and disclosure scan always run.
        result = async {
            process.spawn().await?;
            run_lane(&mut process, &config, &secrets).await
        } => result,
        _ = shutdown_signal() => Err(anyhow!("harness interrupted")),
    };
    let stop_result = process.stop().await;
    let log_path = process.log_path.clone();
    // `stop` retains ownership on every failure. Drop is the final kill/reap
    // fallback, and the disclosure scan runs only after the log is quiescent.
    drop(process);
    let scan_result = secrets.assert_absent_from(&log_path);
    combine_run_outcomes(lane_result, stop_result, scan_result)?;

    eprintln!("matrix-oauth({mode}): passed");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

async fn run_lane(
    process: &mut AxonProcess,
    config: &Config,
    secrets: &SecretTracker,
) -> Result<()> {
    let peer = seed_trusted_peer(config, secrets).await?;
    let approver = MasApprover {
        base: config.mas_base.clone(),
        username: "alice".to_owned(),
        password: config.matrix_password.clone(),
    };

    match config.mode.as_str() {
        "api" => run_api_lane(process, &peer, &approver, secrets).await?,
        "acquire" => run_acquire_lane(process, &peer, &approver, secrets).await?,
        "grant" => run_grant_lane(process, &peer, &approver, secrets).await?,
        "unsupported" => run_unsupported_lane(process, &peer, &approver, secrets).await?,
        _ => unreachable!(),
    }
    Ok(())
}

fn combine_run_outcomes(
    lane_result: Result<()>,
    stop_result: Result<()>,
    scan_result: Result<()>,
) -> Result<()> {
    let mut failures = Vec::new();
    if let Err(error) = lane_result {
        failures.push(format!("lane failed: {error}"));
    }
    if let Err(error) = stop_result {
        failures.push(format!("Axon shutdown failed: {error}"));
    }
    if let Err(error) = scan_result {
        failures.push(format!("Axon log disclosure check failed: {error}"));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(failures.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        process::{Command, Stdio},
        sync::mpsc,
        thread,
        time::Duration,
    };

    use anyhow::anyhow;

    use super::{
        assert_mas_decision, bounded_body, combine_run_outcomes, wait_for_health, Approval,
        BODY_LIMIT,
    };

    #[tokio::test]
    async fn bounded_body_rejects_an_unfinished_chunked_response() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("read test server address");
        let (release_send, release_receive) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept test request");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .expect("write test headers");
            let payload = vec![b'x'; BODY_LIMIT + 1];
            write!(stream, "{:x}\r\n", payload.len()).expect("write test chunk length");
            let _ = stream.write_all(&payload);
            let _ = stream.write_all(b"\r\n");
            let _ = stream.flush();
            let _ = release_receive.recv_timeout(Duration::from_secs(5));
        });

        let response = reqwest::get(format!("http://{address}"))
            .await
            .expect("request chunked test response");
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            bounded_body(response, "test read failed", "test body exceeded limit"),
        )
        .await;
        let _ = release_send.send(());
        server.join().expect("join test server");

        let error = result
            .expect("body limit must be enforced before the stream ends")
            .expect_err("oversized chunked body must fail")
            .to_string();
        assert_eq!(error, "test body exceeded limit");
    }

    #[tokio::test]
    async fn health_wait_reports_a_child_that_exited() {
        let mut child = Command::new(std::env::current_exe().expect("find test executable"))
            .arg("--help")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn short-lived test child");
        tokio::time::sleep(Duration::from_millis(100)).await;

        let error = tokio::time::timeout(Duration::from_secs(2), wait_for_health(9, &mut child))
            .await
            .expect("exited child must be reported without the startup timeout")
            .expect_err("exited child must fail its health wait")
            .to_string();
        let _ = child.wait();

        assert!(error.starts_with("axon-server exited before becoming healthy with "));
    }

    #[test]
    fn run_outcomes_retain_lane_and_disclosure_failures() {
        let error = combine_run_outcomes(
            Err(anyhow!("stable lane failure")),
            Ok(()),
            Err(anyhow!("stable disclosure failure")),
        )
        .expect_err("combined failures must fail the lane")
        .to_string();

        assert!(error.contains("lane failed: stable lane failure"));
        assert!(error.contains("Axon log disclosure check failed: stable disclosure failure"));
    }

    #[test]
    fn mas_decision_requires_the_action_specific_result_element() {
        let consent = r#"<div class="icon success"><svg></svg></div>"#;
        let rejected = r#"<div class="icon invalid"><svg></svg></div>"#;

        assert!(assert_mas_decision(consent, Approval::Consent).is_ok());
        assert!(assert_mas_decision(rejected, Approval::Reject).is_ok());
        assert!(assert_mas_decision(consent, Approval::Reject).is_err());
        assert!(assert_mas_decision(rejected, Approval::Consent).is_err());
    }

    #[test]
    fn mas_decision_rejects_generic_success_text_and_pending_forms() {
        assert!(
            assert_mas_decision(r#"<p class="text-success">success</p>"#, Approval::Consent)
                .is_err()
        );
        assert!(assert_mas_decision(
            r#"<div class="icon success"></div><button name="action">approve</button>"#,
            Approval::Consent
        )
        .is_err());
    }
}
