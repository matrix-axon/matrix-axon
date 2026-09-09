use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context};
use serde::Serialize;
use serde_json::Value;

use crate::axon::{AxonClient, JournalEntry as AxonJournalEntry};
use crate::local_stack::LocalStack;
use crate::matrix::{JournalEntry as MatrixJournalEntry, MatrixClient};
use crate::redact::redact_json;
use crate::wire::Manifest;

type ScenarioFn = for<'a> fn(&'a Ctx) -> Pin<Box<dyn Future<Output = ScenarioOutcome> + Send + 'a>>;

pub struct Ctx {
    pub run_id: String,
    pub timeout: Duration,
    pub manifest: Manifest,
    pub axon: AxonClient,
    pub matrix: MatrixClient,
    pub owns_stack: bool,
    artifacts_dir: PathBuf,
    ws_frames: Arc<Mutex<Vec<Value>>>,
}

pub struct ScenarioOutcome {
    pub result: anyhow::Result<()>,
}

impl ScenarioOutcome {
    pub fn from_result(result: anyhow::Result<()>) -> Self {
        Self { result }
    }
}

impl Ctx {
    pub fn record_ws_frames(&self, frames: Vec<Value>) {
        self.ws_frames.lock().expect("ws frame lock").extend(frames);
    }

    fn clear_ws_frames(&self) {
        self.ws_frames.lock().expect("ws frame lock").clear();
    }

    fn write_artifacts(&self, scenario: &str) -> anyhow::Result<PathBuf> {
        let dir = self.artifacts_dir.join(scenario);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        write_json(dir.join("axon-http-journal.json"), &self.axon.journal())?;
        write_json(dir.join("matrix-http-journal.json"), &self.matrix.journal())?;
        let ws_frames: Vec<_> = self
            .ws_frames
            .lock()
            .expect("ws frame lock")
            .iter()
            .cloned()
            .map(redact_json)
            .collect();
        write_json(dir.join("websocket-frames.json"), &ws_frames)?;
        write_json(
            dir.join("local-stack-manifest.redacted.json"),
            &redacted_manifest(&self.manifest)?,
        )?;
        std::fs::write(dir.join("axon.log.tail"), axon_log_tail(&self.manifest))
            .context("write axon log tail")?;
        Ok(dir)
    }
}

pub async fn run(
    profile: &str,
    filter: Option<&str>,
    manifest_path: Option<PathBuf>,
) -> anyhow::Result<i32> {
    if profile != "true-local" {
        bail!("unknown profile {profile:?}; expected `true-local`");
    }
    let scenarios: Vec<(&str, ScenarioFn)> = vec![
        ("boot_health", |c| {
            Box::pin(crate::scenarios::boot_health(c))
        }),
        ("account_visible", |c| {
            Box::pin(crate::scenarios::account_visible(c))
        }),
        ("room_list", |c| Box::pin(crate::scenarios::room_list(c))),
        ("timeline_read", |c| {
            Box::pin(crate::scenarios::timeline_read(c))
        }),
        ("inbound_timeline_ws", |c| {
            Box::pin(crate::scenarios::inbound_timeline_ws(c))
        }),
        ("outbound_send", |c| {
            Box::pin(crate::scenarios::outbound_send(c))
        }),
        ("unread_counts", |c| {
            Box::pin(crate::scenarios::unread_counts(c))
        }),
        ("relation_reads", |c| {
            Box::pin(crate::scenarios::relation_reads(c))
        }),
        ("media_thread_member", |c| {
            Box::pin(crate::scenarios::media_thread_member(c))
        }),
        ("graceful_stack_shutdown", |c| {
            Box::pin(crate::scenarios::graceful_stack_shutdown(c))
        }),
    ];
    let selected: Vec<&(&str, ScenarioFn)> = match filter {
        Some(needle) => {
            let matched: Vec<_> = scenarios
                .iter()
                .filter(|(name, _)| name.contains(needle))
                .collect();
            if matched.is_empty() {
                bail!("--filter {needle:?} matched no scenario");
            }
            matched
        }
        None => scenarios.iter().collect(),
    };

    let run_id = uuid::Uuid::new_v4().to_string();
    let keep_up = std::env::var("KEEP_UP").ok().as_deref() == Some("1");
    let timeout = Duration::from_secs(
        std::env::var("SMOKE_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(20)
            .max(5),
    );
    let artifacts_dir = artifacts_root()?.join("server").join(&run_id);
    let stack = match manifest_path {
        Some(path) => LocalStack::attach(path)?,
        None => LocalStack::up(&run_id, artifacts_dir.join("local-stack.json"), keep_up)?,
    };
    let manifest = stack.manifest.clone();
    let ctx = Ctx {
        run_id: run_id.clone(),
        timeout,
        axon: AxonClient::new(
            manifest.axon_base_url.clone(),
            manifest.axon_bearer_token.clone(),
        )?,
        matrix: MatrixClient::new(manifest.homeserver_url.clone()),
        manifest,
        owns_stack: stack.owns_stack(),
        artifacts_dir: artifacts_dir.clone(),
        ws_frames: Arc::new(Mutex::new(Vec::new())),
    };
    if let Err(err) = crate::scenarios::seeded_fixtures_ready(&ctx).await {
        eprintln!("smoke(server): ✗ seeded fixture readiness: {err:#}");
        match ctx.write_artifacts("seeded-fixture-readiness") {
            Ok(dir) => eprintln!("smoke(server):   artifacts → {}", dir.display()),
            Err(write_err) => {
                eprintln!("smoke(server):   (failed to write artifacts: {write_err:#})")
            }
        }
        if let Err(teardown_err) = stack.down() {
            return Err(teardown_err.context("tear down stack after fixture readiness failure"));
        }
        return Err(err);
    }
    eprintln!(
        "smoke(server): run {run_id}, profile={profile}, {} scenario(s), axon {}",
        selected.len(),
        ctx.manifest.axon_base_url
    );

    let mut failures = 0usize;
    for (name, scenario) in &selected {
        ctx.clear_ws_frames();
        eprintln!("smoke(server): ▶ {name}");
        let outcome = scenario(&ctx).await;
        match &outcome.result {
            Ok(()) => eprintln!("smoke(server): ✓ {name}"),
            Err(err) => {
                failures += 1;
                eprintln!("smoke(server): ✗ {name}: {err:#}");
                match ctx.write_artifacts(name) {
                    Ok(dir) => eprintln!("smoke(server):   artifacts → {}", dir.display()),
                    Err(e) => eprintln!("smoke(server):   (failed to write artifacts: {e:#})"),
                }
            }
        }
    }

    let down_result = stack.down();
    if let Err(err) = down_result {
        failures += 1;
        eprintln!("smoke(server): ✗ stack teardown: {err:#}");
    }
    if failures == 0 && !keep_up && stack.owns_stack() {
        let _ = std::fs::remove_dir_all(&artifacts_dir);
    }
    eprintln!(
        "smoke(server): {} passed, {} failed of {}",
        selected.len() - failures.min(selected.len()),
        failures,
        selected.len()
    );
    Ok(if failures == 0 { 0 } else { 1 })
}

fn artifacts_root() -> anyhow::Result<PathBuf> {
    Ok(workspace_root()?.join("smoke-artifacts"))
}

fn workspace_root() -> anyhow::Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("cannot derive workspace root from {}", manifest.display()))
}

fn write_json<T: Serialize>(path: PathBuf, value: &T) -> anyhow::Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(value)? + "\n").context("write JSON artifact")
}

fn axon_log_tail(manifest: &Manifest) -> String {
    let text = std::fs::read_to_string(&manifest.paths.axon_log).unwrap_or_default();
    let lines: Vec<_> = text.lines().rev().take(300).collect();
    lines.into_iter().rev().collect::<Vec<_>>().join("\n")
}

fn redacted_manifest(manifest: &Manifest) -> anyhow::Result<Value> {
    let mut value = serde_json::to_value(manifest)?;
    if let Some(token) = value.get_mut("axon_bearer_token") {
        *token = Value::String("<redacted>".to_owned());
    }
    if let Some(accounts) = value.get_mut("accounts").and_then(Value::as_object_mut) {
        for account in accounts.values_mut() {
            if let Some(password) = account.get_mut("password") {
                *password = Value::String("<redacted>".to_owned());
            }
        }
    }
    if let Some(manual) = value.get_mut("manual").and_then(Value::as_object_mut) {
        if let Some(command) = manual.get_mut("tui_command") {
            *command = Value::String("<redacted>".to_owned());
        }
    }
    Ok(value)
}

impl Serialize for AxonJournalEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("AxonJournalEntry", 6)?;
        state.serialize_field("method", &self.method)?;
        state.serialize_field("url", &self.url)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("request_body", &self.request_body)?;
        state.serialize_field("response_body", &self.response_body)?;
        state.serialize_field("error", &self.error)?;
        state.end()
    }
}

impl Serialize for MatrixJournalEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("MatrixJournalEntry", 6)?;
        state.serialize_field("method", &self.method)?;
        state.serialize_field("url", &self.url)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("request_body", &self.request_body)?;
        state.serialize_field("response_body", &self.response_body)?;
        state.serialize_field("error", &self.error)?;
        state.end()
    }
}

use serde::ser::SerializeStruct;
