use std::fs::File;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use clap::{Parser, Subcommand};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod corpus;

use corpus::DemoManifest;

#[cfg(unix)]
unsafe extern "C" {
    fn setsid() -> i32;
}

const AS_TOKEN: &str = "axon-smoke-appservice-token";
const AXON_TIMELINE_LIMIT: &str = "200";
const LONG_TIMELINE_MESSAGES_PER_DATE: i64 = 15;
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);
const SERVER_NAME: &str = "localhost";

#[derive(Parser)]
#[command(name = "axon-smoke-local-stack")]
struct Args {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Subcommand)]
enum CommandKind {
    Up {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        keep_up: bool,
        #[arg(long)]
        quiet: bool,
        /// Additionally render a demo corpus (ADR 0086) into the stack, and log
        /// axon in as its viewer. Unset by default: every existing smoke
        /// scenario gets exactly the stack it always got.
        #[arg(long)]
        corpus: Option<PathBuf>,
    },
    Down {
        #[arg(long)]
        manifest: PathBuf,
    },
    Info {
        #[arg(long)]
        manifest: PathBuf,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    run_id: String,
    axon_base_url: String,
    axon_bearer_token: String,
    homeserver_url: String,
    axon_account_id: Uuid,
    manual: Manual,
    accounts: Accounts,
    rooms: Rooms,
    fixtures: Fixtures,
    /// Present only for a `--corpus` run. Absent keeps the manifest byte-for-byte
    /// what every existing scenario already reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    demo: Option<DemoManifest>,
    paths: Paths,
    keep_up: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manual {
    tui_command: String,
    matrix_client_homeserver: String,
    notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Accounts {
    target: MatrixAccount,
    peer: MatrixAccount,
    observer: MatrixAccount,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MatrixAccount {
    user_id: String,
    password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Rooms {
    general: RoomInfo,
    long_timeline: TimelineRoomInfo,
    relations: RoomInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RoomInfo {
    room_id: String,
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TimelineRoomInfo {
    room_id: String,
    name: String,
    jump_dates: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Fixtures {
    relations: RelationFixtures,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RelationFixtures {
    root_event_id: String,
    reply_event_id: String,
    edit_event_id: String,
    peer_reaction_event_id: String,
    own_reaction_event_id: String,
    formatted_event_id: String,
    redacted_event_id: String,
    redaction_event_id: String,
    thread_root_event_id: String,
    thread_member_event_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Paths {
    run_dir: PathBuf,
    axon_log: PathBuf,
    compose_project: String,
    database_name: String,
    axon_pid: u32,
    postgres_port: u16,
    synapse_port: u16,
    axon_port: u16,
}

#[derive(Debug, Deserialize)]
struct CleanupManifest {
    paths: Paths,
}

#[derive(Debug, Deserialize)]
struct RoomResponse {
    room_id: String,
}

#[derive(Debug, Deserialize)]
struct LoginResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct AxonEnvelope<T> {
    data: T,
}

#[derive(Debug, Deserialize)]
struct AxonAccount {
    account_id: Uuid,
}

#[derive(Clone, Copy)]
struct PortOverrides {
    postgres: Option<u16>,
    synapse: Option<u16>,
    axon: Option<u16>,
}

impl PortOverrides {
    fn read() -> anyhow::Result<Self> {
        Ok(Self {
            postgres: env_port("SMOKE_POSTGRES_PORT")?,
            synapse: env_port("SMOKE_SYNAPSE_PORT")?,
            axon: env_port("SMOKE_AXON_PORT")?,
        })
    }

    fn has_explicit_port(self) -> bool {
        self.postgres.is_some() || self.synapse.is_some() || self.axon.is_some()
    }

    fn choose(self) -> anyhow::Result<PortSet> {
        Ok(PortSet {
            postgres: self.postgres.unwrap_or(free_port()?),
            synapse: self.synapse.unwrap_or(free_port()?),
            axon: self.axon.unwrap_or(free_port()?),
        })
    }
}

#[derive(Clone, Copy)]
struct PortSet {
    postgres: u16,
    synapse: u16,
    axon: u16,
}

struct UpCleanup {
    paths: Paths,
    armed: bool,
}

impl UpCleanup {
    fn new(paths: Paths) -> Self {
        Self { paths, armed: true }
    }

    fn set_axon_pid(&mut self, axon_pid: u32) {
        self.paths.axon_pid = axon_pid;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for UpCleanup {
    fn drop(&mut self) {
        if self.armed {
            cleanup_paths(&self.paths);
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.command {
        CommandKind::Up {
            manifest,
            run_id,
            keep_up,
            quiet,
            corpus,
        } => up(&manifest, run_id, keep_up, quiet, corpus.as_deref()).await,
        CommandKind::Down { manifest } => down(&manifest).await,
        CommandKind::Info { manifest } => info(&manifest),
    }
}

async fn up(
    path: &Path,
    run_id: Option<String>,
    keep_up: bool,
    quiet: bool,
    corpus_path: Option<&Path>,
) -> anyhow::Result<()> {
    let run_id = run_id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let overrides = PortOverrides::read()?;
    let attempts = if overrides.has_explicit_port() { 1 } else { 3 };
    let mut last_error = None;
    for attempt in 1..=attempts {
        let ports = overrides.choose()?;
        match up_attempt(path, &run_id, keep_up, quiet, ports, corpus_path).await {
            Ok(()) => return Ok(()),
            Err(err) if attempt < attempts && is_port_collision(&err) => {
                eprintln!(
                    "axon-smoke-local-stack: auto-selected port collision on attempt {attempt}; retrying with new ports: {err:#}"
                );
                last_error = Some(err);
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("local stack setup exhausted port retries")))
}

async fn up_attempt(
    path: &Path,
    run_id: &str,
    keep_up: bool,
    quiet: bool,
    ports: PortSet,
    corpus_path: Option<&Path>,
) -> anyhow::Result<()> {
    let short = run_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect::<String>();
    let postgres_port = ports.postgres;
    let synapse_port = ports.synapse;
    let axon_port = ports.axon;
    let compose_project = format!("axon-smoke-{short}");
    let database_name = format!("axon_smoke_{short}");
    let run_dir = std::env::temp_dir().join(format!("axon-smoke-local-stack-{run_id}"));
    std::fs::create_dir_all(&run_dir).with_context(|| format!("create {}", run_dir.display()))?;
    let axon_log = run_dir.join("axon.log");
    let mut cleanup = UpCleanup::new(Paths {
        run_dir: run_dir.clone(),
        axon_log: axon_log.clone(),
        compose_project: compose_project.clone(),
        database_name: database_name.clone(),
        axon_pid: 0,
        postgres_port,
        synapse_port,
        axon_port,
    });
    let compose_override = prepare_synapse_config(&run_dir)?;

    compose_up(
        &compose_project,
        &compose_override,
        postgres_port,
        synapse_port,
    )?;
    wait_for_postgres(&compose_project).await?;
    create_database(&compose_project, &database_name)?;
    let hs_url = format!("http://127.0.0.1:{synapse_port}");
    wait_for_synapse(&hs_url).await?;

    let accounts = Accounts {
        target: account("axon", &short),
        peer: account("peer", &short),
        observer: account("observer", &short),
    };
    register_user(&compose_project, &accounts.target)?;
    register_user(&compose_project, &accounts.peer)?;
    register_user(&compose_project, &accounts.observer)?;

    let client = http_client()?;
    let target_token = login_matrix(&client, &hs_url, &accounts.target).await?;
    let peer_token = login_matrix(&client, &hs_url, &accounts.peer).await?;
    let observer_token = login_matrix(&client, &hs_url, &accounts.observer).await?;

    let general = create_joined_room(
        &client,
        &hs_url,
        "Smoke General",
        &target_token,
        &[
            (&accounts.peer, &peer_token),
            (&accounts.observer, &observer_token),
        ],
    )
    .await?;
    seed_general(
        &client,
        &hs_url,
        &general.room_id,
        &target_token,
        &peer_token,
    )
    .await?;

    let long_timeline = create_joined_room(
        &client,
        &hs_url,
        "Smoke Timeline",
        &target_token,
        &[(&accounts.peer, &peer_token)],
    )
    .await?;
    let jump_dates = seed_long_timeline(
        &client,
        &hs_url,
        &long_timeline.room_id,
        &target_token,
        &short,
    )
    .await?;

    let relations = create_joined_room(
        &client,
        &hs_url,
        "Smoke Relations",
        &target_token,
        &[(&accounts.peer, &peer_token)],
    )
    .await?;
    let relation_fixtures = seed_relations(
        &client,
        &hs_url,
        &relations.room_id,
        &target_token,
        &peer_token,
    )
    .await?;

    // The demo corpus is additive: the smoke fixtures above are seeded exactly
    // as always, and only the account axon logs in as changes — a demo is
    // recorded through the corpus viewer's eyes, and it must not have the
    // `Smoke *` rooms in its room list.
    let demo = match corpus_path {
        Some(path) => Some(
            corpus::seed(&client, &hs_url, &compose_project, path, &short)
                .await
                .with_context(|| format!("seed corpus {}", path.display()))?,
        ),
        None => None,
    };
    let (axon_account, sync_probe_room) = match &demo {
        Some(demo) => (demo.viewer.clone(), demo.sync_probe_room.clone()),
        None => (accounts.target.clone(), long_timeline.room_id.clone()),
    };

    let axon_bin = resolve_axon_bin()?;
    let database_url = format!("postgres://axon:axon@127.0.0.1:{postgres_port}/{database_name}");
    let axon_pid = start_axon(&axon_bin, &run_dir, &axon_log, &database_url, axon_port)?;
    cleanup.set_axon_pid(axon_pid);
    let axon_base_url = format!("http://127.0.0.1:{axon_port}");
    wait_for_axon(&client, &axon_base_url).await?;
    let token = issue_axon_token(&axon_bin, &run_dir, &database_url)?;
    let account_id = login_axon(&client, &axon_base_url, &token, &axon_account, &hs_url).await?;
    wait_for_axon_room(&client, &axon_base_url, &token, &sync_probe_room).await?;

    let manifest = Manifest {
        run_id: run_id.to_owned(),
        axon_base_url: axon_base_url.clone(),
        axon_bearer_token: token.clone(),
        homeserver_url: hs_url.clone(),
        axon_account_id: account_id,
        manual: Manual {
            tui_command: format!("AXON_BASE_URL={axon_base_url} AXON_TOKEN={token} axon-tui"),
            matrix_client_homeserver: hs_url.clone(),
            notes: "Throwaway local-only credentials; safe to inspect, not safe to reuse."
                .to_owned(),
        },
        accounts,
        rooms: Rooms {
            general,
            long_timeline: TimelineRoomInfo {
                room_id: long_timeline.room_id,
                name: long_timeline.name,
                jump_dates,
            },
            relations,
        },
        fixtures: Fixtures {
            relations: relation_fixtures,
        },
        demo: demo.map(|demo| demo.manifest),
        paths: cleanup.paths.clone(),
        keep_up,
    };

    write_manifest(path, &manifest)?;
    cleanup.disarm();
    if !quiet {
        print_info(&manifest);
    }
    Ok(())
}

async fn down(path: &Path) -> anyhow::Result<()> {
    let manifest = read_cleanup_manifest(path)?;
    cleanup_paths(&manifest.paths);
    Ok(())
}

fn cleanup_paths(paths: &Paths) {
    if paths.axon_pid != 0 {
        let _ = Command::new("kill")
            .arg(paths.axon_pid.to_string())
            .status();
    }
    let _ = drop_database(&paths.compose_project, &paths.database_name);
    let _ = Command::new("docker")
        .args([
            "compose",
            "-p",
            &paths.compose_project,
            "--profile",
            "integration",
            "down",
            "-v",
        ])
        .status();
    let _ = std::fs::remove_dir_all(&paths.run_dir);
}

fn info(path: &Path) -> anyhow::Result<()> {
    let manifest = read_manifest(path)?;
    print_info(&manifest);
    Ok(())
}

fn print_info(manifest: &Manifest) {
    println!("Run ID: {}", manifest.run_id);
    println!("Axon: {}", manifest.axon_base_url);
    println!("Axon token: {}", manifest.axon_bearer_token);
    println!("Manual TUI: {}", manifest.manual.tui_command);
    println!("Synapse: {}", manifest.homeserver_url);
    println!("Matrix accounts:");
    for (label, account) in [
        ("target", &manifest.accounts.target),
        ("peer", &manifest.accounts.peer),
        ("observer", &manifest.accounts.observer),
    ] {
        println!("  {label}: {} / {}", account.user_id, account.password);
    }
    println!("Rooms:");
    println!(
        "  general: {} ({})",
        manifest.rooms.general.name, manifest.rooms.general.room_id
    );
    println!(
        "  long_timeline: {} ({}) jump dates: {}",
        manifest.rooms.long_timeline.name,
        manifest.rooms.long_timeline.room_id,
        manifest.rooms.long_timeline.jump_dates.join(", ")
    );
    println!(
        "  relations: {} ({})",
        manifest.rooms.relations.name, manifest.rooms.relations.room_id
    );
    if let Some(demo) = &manifest.demo {
        println!(
            "Demo corpus: {} ({})",
            demo.name,
            demo.corpus_path.display()
        );
        println!(
            "  viewer: {} ({})",
            demo.viewer.account.user_id, demo.viewer.persona
        );
        for (id, space) in &demo.spaces {
            println!(
                "  space {id}: {} ({}) children: {}",
                space.name,
                space.room_id,
                space.children.join(", ")
            );
        }
        for (id, room) in &demo.rooms {
            let kind = if room.direct { " [dm]" } else { "" };
            println!("  room {id}{kind}: {}", room.room_id);
        }
        println!("  media uploaded: {}", demo.media.len());
    }
    println!("Cleanup: cargo run -p axon-smoke-local-stack -- down --manifest <manifest>");
}

fn account(prefix: &str, short: &str) -> MatrixAccount {
    MatrixAccount {
        user_id: format!("@{prefix}-{short}:{SERVER_NAME}"),
        password: format!("pass-{prefix}-{short}"),
    }
}

fn env_port(key: &str) -> anyhow::Result<Option<u16>> {
    match std::env::var(key) {
        Ok(value) if !value.is_empty() => {
            Ok(Some(value.parse().with_context(|| format!("parse {key}"))?))
        }
        _ => Ok(None),
    }
}

fn free_port() -> anyhow::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("bind free port")?;
    Ok(listener.local_addr()?.port())
}

fn is_port_collision(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_ascii_lowercase();
    [
        "address already in use",
        "port is already allocated",
        "ports are not available",
        "failed to bind",
        "addrinuse",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn prepare_synapse_config(run_dir: &Path) -> anyhow::Result<PathBuf> {
    let root = workspace_root()?;
    let base_homeserver = root.join("docker/synapse/homeserver.yaml");
    let mut homeserver = std::fs::read_to_string(&base_homeserver)
        .with_context(|| format!("read {}", base_homeserver.display()))?;
    if !homeserver.contains("app_service_config_files:") {
        homeserver.push_str(
            r#"
# Smoke-only appservice used for timestamped fixture messages.
app_service_config_files:
  - /data/appservice-smoke.yaml
"#,
        );
    }
    if !homeserver.contains("rc_invites:") {
        // The base config opens up messages, registration and login, but not
        // invites or joins — which default to a fraction of one per second and
        // so 429 a corpus that stands up seven rooms of members in a row.
        homeserver.push_str(
            r#"
# Smoke-only: seeding invites and joins a whole fictional membership at once.
rc_invites:
  per_room:
    per_second: 1000
    burst_count: 1000
  per_user:
    per_second: 1000
    burst_count: 1000
  per_issuer:
    per_second: 1000
    burst_count: 1000
rc_joins:
  local:
    per_second: 1000
    burst_count: 1000
  remote:
    per_second: 1000
    burst_count: 1000
"#,
        );
    }

    let homeserver_path = run_dir.join("homeserver.yaml");
    let appservice_path = run_dir.join("appservice-smoke.yaml");
    let compose_override = run_dir.join("compose.override.yaml");
    std::fs::write(&homeserver_path, homeserver)
        .with_context(|| format!("write {}", homeserver_path.display()))?;
    std::fs::write(
        &appservice_path,
        r#"id: axon-smoke-appservice
url: null
as_token: "axon-smoke-appservice-token"
hs_token: "axon-smoke-homeserver-token"
sender_localpart: "smokeas_bot"
rate_limited: false
namespaces:
  users:
    - exclusive: true
      regex: "@smokeas_.*:localhost"
    # Non-exclusive wildcard: `?user_id=` impersonation is only permitted for
    # users the appservice is "interested in", and the demo corpus wants clean
    # persona IDs (@maya:localhost) rather than namespace-prefixed ones. Staying
    # non-exclusive is what lets register_new_matrix_user still create those
    # same users with passwords. Test-only, on a disposable homeserver whose
    # registration file is generated per run — see ADR 0086.
    - exclusive: false
      regex: "@.*:localhost"
  aliases: []
  rooms: []
"#,
    )
    .with_context(|| format!("write {}", appservice_path.display()))?;

    let homeserver_mount = serde_json::to_string(&format!(
        "{}:/data/homeserver.yaml:ro",
        homeserver_path.display()
    ))?;
    let appservice_mount = serde_json::to_string(&format!(
        "{}:/data/appservice-smoke.yaml:ro",
        appservice_path.display()
    ))?;
    std::fs::write(
        &compose_override,
        format!(
            "services:\n  synapse:\n    volumes:\n      - {homeserver_mount}\n      - {appservice_mount}\n"
        ),
    )
    .with_context(|| format!("write {}", compose_override.display()))?;

    Ok(compose_override)
}

/// Retry `op` up to `attempts` times, sleeping `backoff * attempt` between tries.
///
/// Used for the Docker steps that reach Docker Hub: an anonymous image pull can
/// time out or hit the unauthenticated rate limit, and a single transient
/// failure should not red the smoke gate.
fn with_retries(
    attempts: u32,
    backoff: Duration,
    label: &str,
    mut op: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut last_error = None;
    for attempt in 1..=attempts {
        match op() {
            Ok(()) => return Ok(()),
            Err(err) if attempt < attempts => {
                let wait = backoff * attempt;
                eprintln!(
                    "axon-smoke-local-stack: {label} failed on attempt {attempt}/{attempts}; retrying in {wait:?}: {err:#}"
                );
                std::thread::sleep(wait);
                last_error = Some(err);
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("{label} exhausted {attempts} retries")))
}

fn compose_up(
    project: &str,
    compose_override: &Path,
    postgres_port: u16,
    synapse_port: u16,
) -> anyhow::Result<()> {
    let base_compose = workspace_root()?.join("docker-compose.yml");
    let base_compose = base_compose
        .to_str()
        .ok_or_else(|| anyhow!("docker-compose.yml path is not UTF-8"))?
        .to_owned();
    let override_path = compose_override
        .to_str()
        .ok_or_else(|| anyhow!("compose override path is not UTF-8"))?
        .to_owned();

    // `-f base -f override -p project --profile integration`, shared by every
    // compose invocation below.
    let compose = || {
        let mut cmd = Command::new("docker");
        cmd.args([
            "compose",
            "-f",
            base_compose.as_str(),
            "-f",
            override_path.as_str(),
            "-p",
            project,
            "--profile",
            "integration",
        ]);
        cmd
    };

    // Pull first, with retries. `up -d` below still fetches anything missed, so
    // this is belt-and-braces against a flaky registry, not the only fetch.
    with_retries(3, Duration::from_secs(5), "docker compose pull", || {
        command_status(
            compose().args(["pull", "--quiet", "postgres", "synapse"]),
            "docker compose pull",
        )
    })?;

    with_retries(3, Duration::from_secs(5), "docker compose up", || {
        command_status(
            compose()
                .args(["up", "-d", "postgres", "synapse"])
                .env("POSTGRES_PORT", postgres_port.to_string())
                .env("SYNAPSE_PORT", synapse_port.to_string()),
            "docker compose up",
        )
        .inspect_err(|_| {
            // Tear down a half-started project so the retry starts clean.
            let _ = command_status(compose().args(["down", "-v"]), "docker compose down");
        })
    })
}

fn compose_container(project: &str, service: &str) -> anyhow::Result<String> {
    let output = Command::new("docker")
        .args(["compose", "-p", project, "ps", "-q", service])
        .output()
        .with_context(|| format!("docker compose ps {service}"))?;
    if !output.status.success() {
        bail!("docker compose ps {service} failed");
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if id.is_empty() {
        bail!("container id for {service} is empty");
    }
    Ok(id)
}

async fn wait_for_postgres(project: &str) -> anyhow::Result<()> {
    let pg = compose_container(project, "postgres")?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let status = Command::new("docker")
            .args(["exec", &pg, "pg_isready", "-U", "axon", "-d", "axon"])
            .status()
            .context("pg_isready")?;
        if status.success() {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("Postgres did not become ready");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn create_database(project: &str, db: &str) -> anyhow::Result<()> {
    psql(
        project,
        &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE);"),
    )?;
    psql(project, &format!("CREATE DATABASE {db};"))
}

fn drop_database(project: &str, db: &str) -> anyhow::Result<()> {
    psql(
        project,
        &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE);"),
    )
}

fn psql(project: &str, sql: &str) -> anyhow::Result<()> {
    let pg = compose_container(project, "postgres")?;
    command_status(
        Command::new("docker").args([
            "exec", "-i", &pg, "psql", "-U", "axon", "-d", "axon", "-tAc", sql,
        ]),
        "psql",
    )
}

async fn wait_for_synapse(client_url: &str) -> anyhow::Result<()> {
    let client = http_client()?;
    let url = format!("{client_url}/_matrix/client/versions");
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Ok(resp) = client.get(&url).timeout(HTTP_REQUEST_TIMEOUT).send().await {
            if resp.status().is_success() {
                let text = resp.text().await.unwrap_or_default();
                if text.contains("org.matrix.simplified_msc3575") {
                    return Ok(());
                }
            }
        }
        if Instant::now() > deadline {
            bail!("Synapse did not become ready with MSC4186");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn register_user(project: &str, account: &MatrixAccount) -> anyhow::Result<()> {
    let synapse = compose_container(project, "synapse")?;
    let localpart = account
        .user_id
        .strip_prefix('@')
        .and_then(|s| s.strip_suffix(":localhost"))
        .ok_or_else(|| anyhow!("invalid local user id {}", account.user_id))?;
    command_status(
        Command::new("docker").args([
            "exec",
            &synapse,
            "register_new_matrix_user",
            "-c",
            "/data/homeserver.yaml",
            "-u",
            localpart,
            "-p",
            &account.password,
            "--no-admin",
            "http://localhost:8008",
        ]),
        "register_new_matrix_user",
    )
}

async fn login_matrix(
    client: &Client,
    hs: &str,
    account: &MatrixAccount,
) -> anyhow::Result<String> {
    let resp: LoginResponse = client
        .post(format!("{hs}/_matrix/client/v3/login"))
        .json(&serde_json::json!({
            "type": "m.login.password",
            "identifier": { "type": "m.id.user", "user": account.user_id },
            "password": account.password
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp.access_token)
}

async fn create_joined_room(
    client: &Client,
    hs: &str,
    name: &str,
    creator_token: &str,
    joiners: &[(&MatrixAccount, &String)],
) -> anyhow::Result<RoomInfo> {
    let resp: RoomResponse = client
        .post(format!("{hs}/_matrix/client/v3/createRoom"))
        .bearer_auth(creator_token)
        .json(&serde_json::json!({ "name": name, "preset": "public_chat", "visibility": "public" }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    for (account, token) in joiners {
        invite_user(client, hs, &resp.room_id, creator_token, &account.user_id).await?;
        client
            .post(format!(
                "{hs}/_matrix/client/v3/join/{}",
                url_path(&resp.room_id)
            ))
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?;
    }
    Ok(RoomInfo {
        room_id: resp.room_id,
        name: name.to_owned(),
    })
}

async fn invite_user(
    client: &Client,
    hs: &str,
    room: &str,
    inviter_token: &str,
    user_id: &str,
) -> anyhow::Result<()> {
    client
        .post(format!(
            "{hs}/_matrix/client/v3/rooms/{}/invite",
            url_path(room)
        ))
        .bearer_auth(inviter_token)
        .json(&serde_json::json!({ "user_id": user_id }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn seed_general(
    client: &Client,
    hs: &str,
    room: &str,
    target: &str,
    peer: &str,
) -> anyhow::Result<()> {
    for (i, (token, body)) in [
        (target, "general target hello"),
        (peer, "general peer hello"),
        (target, "general target follow-up"),
        (peer, "general peer follow-up"),
    ]
    .into_iter()
    .enumerate()
    {
        send_message(client, hs, room, token, &format!("general-{i}"), body).await?;
    }
    Ok(())
}

async fn seed_long_timeline(
    client: &Client,
    hs: &str,
    room: &str,
    inviter_token: &str,
    short: &str,
) -> anyhow::Result<Vec<String>> {
    let as_user = format!("@smokeas_timeline_{short}:localhost");
    register_appservice_user(client, hs, &as_user).await?;
    invite_user(client, hs, room, inviter_token, &as_user).await?;
    join_appservice_user(client, hs, room, &as_user).await?;
    let dates = ["2026-01-03", "2026-02-14", "2026-04-01", "2026-06-15"];
    let mut count = 0usize;
    for i in 0..LONG_TIMELINE_MESSAGES_PER_DATE {
        for date in dates {
            let base = date_ms(date)?;
            let ts = base + i * 60_000;
            let body = format!("jump fixture {date} message {i:02}");
            send_appservice_message(
                client,
                hs,
                room,
                &as_user,
                &format!("long-{date}-{i}"),
                &body,
                ts,
            )
            .await?;
            count += 1;
        }
    }
    if count <= 50 {
        bail!("long timeline fixture seeded too few messages");
    }
    Ok(dates.iter().map(|s| s.to_string()).collect())
}

async fn seed_relations(
    client: &Client,
    hs: &str,
    room: &str,
    target: &str,
    peer: &str,
) -> anyhow::Result<RelationFixtures> {
    let root = send_message(
        client,
        hs,
        room,
        target,
        "relations-root",
        "relations root message",
    )
    .await?;
    let reply_content = serde_json::json!({
        "msgtype": "m.text",
        "body": "relations reply message",
        "m.relates_to": { "m.in_reply_to": { "event_id": root } }
    });
    let reply = send_raw(
        client,
        hs,
        room,
        peer,
        "relations-reply",
        reply_content,
        None,
    )
    .await?;
    let edit_content = serde_json::json!({
        "msgtype": "m.text",
        "body": "* relations root edited",
        "m.new_content": { "msgtype": "m.text", "body": "relations root edited" },
        "m.relates_to": { "rel_type": "m.replace", "event_id": root }
    });
    let edit = send_raw(
        client,
        hs,
        room,
        target,
        "relations-edit",
        edit_content,
        None,
    )
    .await?;
    let reaction_content = serde_json::json!({
        "m.relates_to": { "rel_type": "m.annotation", "event_id": root, "key": "👍" }
    });
    let peer_reaction = send_raw(
        client,
        hs,
        room,
        peer,
        "relations-reaction",
        reaction_content,
        Some("m.reaction"),
    )
    .await?;
    let own_reaction_content = serde_json::json!({
        "m.relates_to": { "rel_type": "m.annotation", "event_id": root, "key": "✅" }
    });
    let own_reaction = send_raw(
        client,
        hs,
        room,
        target,
        "relations-own-reaction",
        own_reaction_content,
        Some("m.reaction"),
    )
    .await?;
    let formatted = serde_json::json!({
        "msgtype": "m.text",
        "body": "formatted bold link",
        "format": "org.matrix.custom.html",
        "formatted_body": "<strong>formatted</strong> <a href=\"https://example.com\">link</a>"
    });
    let formatted_event = send_raw(
        client,
        hs,
        room,
        peer,
        "relations-formatted",
        formatted.clone(),
        None,
    )
    .await?;
    let redacted = send_raw(
        client,
        hs,
        room,
        peer,
        "relations-redacted",
        formatted,
        None,
    )
    .await?;
    let redaction_resp: serde_json::Value = client
        .put(format!(
            "{hs}/_matrix/client/v3/rooms/{}/redact/{}/{}",
            url_path(room),
            url_path(&redacted),
            "relations-redact"
        ))
        .bearer_auth(target)
        .json(&serde_json::json!({ "reason": "smoke redaction" }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let redaction = redaction_resp
        .get("event_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("redaction response missing event_id"))?;

    let thread_root = send_message(
        client,
        hs,
        room,
        target,
        "relations-thread-root",
        "relations thread root",
    )
    .await?;
    let thread_content = serde_json::json!({
        "msgtype": "m.text",
        "body": "relations thread member",
        "m.relates_to": {
            "rel_type": "m.thread",
            "event_id": thread_root,
            "m.in_reply_to": { "event_id": thread_root }
        }
    });
    let thread_member = send_raw(
        client,
        hs,
        room,
        peer,
        "relations-thread-member",
        thread_content,
        None,
    )
    .await?;

    Ok(RelationFixtures {
        root_event_id: root,
        reply_event_id: reply,
        edit_event_id: edit,
        peer_reaction_event_id: peer_reaction,
        own_reaction_event_id: own_reaction,
        formatted_event_id: formatted_event,
        redacted_event_id: redacted,
        redaction_event_id: redaction,
        thread_root_event_id: thread_root,
        thread_member_event_id: thread_member,
    })
}

async fn send_message(
    client: &Client,
    hs: &str,
    room: &str,
    token: &str,
    txn: &str,
    body: &str,
) -> anyhow::Result<String> {
    send_raw(
        client,
        hs,
        room,
        token,
        txn,
        serde_json::json!({ "msgtype": "m.text", "body": body }),
        None,
    )
    .await
}

async fn send_raw(
    client: &Client,
    hs: &str,
    room: &str,
    token: &str,
    txn: &str,
    content: serde_json::Value,
    event_type: Option<&str>,
) -> anyhow::Result<String> {
    let event_type = event_type.unwrap_or("m.room.message");
    let resp: serde_json::Value = client
        .put(format!(
            "{hs}/_matrix/client/v3/rooms/{}/send/{}/{}",
            url_path(room),
            event_type,
            txn
        ))
        .bearer_auth(token)
        .json(&content)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    resp.get("event_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("send response missing event_id"))
}

async fn join_appservice_user(
    client: &Client,
    hs: &str,
    room: &str,
    user_id: &str,
) -> anyhow::Result<()> {
    client
        .post(format!("{hs}/_matrix/client/v3/join/{}", url_path(room)))
        .bearer_auth(AS_TOKEN)
        .query(&[("user_id", user_id)])
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn register_appservice_user(client: &Client, hs: &str, user_id: &str) -> anyhow::Result<()> {
    let localpart = user_id
        .strip_prefix('@')
        .and_then(|s| s.strip_suffix(":localhost"))
        .ok_or_else(|| anyhow!("invalid appservice user id {user_id}"))?;
    client
        .post(format!("{hs}/_matrix/client/v3/register"))
        .bearer_auth(AS_TOKEN)
        .json(&serde_json::json!({
            "type": "m.login.application_service",
            "username": localpart,
            "inhibit_login": true
        }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn send_appservice_message(
    client: &Client,
    hs: &str,
    room: &str,
    user_id: &str,
    txn: &str,
    body: &str,
    ts: i64,
) -> anyhow::Result<String> {
    send_appservice_event(
        client,
        hs,
        room,
        user_id,
        txn,
        "m.room.message",
        serde_json::json!({ "msgtype": "m.text", "body": body }),
        ts,
    )
    .await
}

/// Send any event as any local user, backdated.
///
/// The `?user_id=&ts=` appservice route is the only way to write an event with
/// a historical `origin_server_ts`, so every fixture that needs to look like it
/// happened last week goes through here — images, threads, reactions,
/// formatted bodies, redactions. `send_appservice_message` is the narrow
/// `m.text` wrapper `seed_long_timeline` has always used.
// The arguments are exactly the send route's own parameters; bundling them into
// a struct would only move the same list to the call sites.
#[allow(clippy::too_many_arguments)]
async fn send_appservice_event(
    client: &Client,
    hs: &str,
    room: &str,
    user_id: &str,
    txn: &str,
    event_type: &str,
    content: serde_json::Value,
    ts: i64,
) -> anyhow::Result<String> {
    let ts = ts.to_string();
    let resp: serde_json::Value = client
        .put(format!(
            "{hs}/_matrix/client/v3/rooms/{}/send/{}/{}",
            url_path(room),
            url_path(event_type),
            txn
        ))
        .bearer_auth(AS_TOKEN)
        .query(&[("user_id", user_id), ("ts", ts.as_str())])
        .json(&content)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    resp.get("event_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("appservice send response missing event_id"))
}

/// Redact an event as any local user, backdated.
async fn redact_appservice_event(
    client: &Client,
    hs: &str,
    room: &str,
    user_id: &str,
    event_id: &str,
    txn: &str,
    ts: i64,
) -> anyhow::Result<String> {
    let ts = ts.to_string();
    let resp: serde_json::Value = client
        .put(format!(
            "{hs}/_matrix/client/v3/rooms/{}/redact/{}/{}",
            url_path(room),
            url_path(event_id),
            txn
        ))
        .bearer_auth(AS_TOKEN)
        .query(&[("user_id", user_id), ("ts", ts.as_str())])
        .json(&serde_json::json!({ "reason": "corpus redaction" }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    resp.get("event_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("appservice redact response missing event_id"))
}

/// Upload bytes to the homeserver's media repository, returning the `mxc://`
/// URI. Nothing else in `smoke/` creates media, so image fixtures — and the
/// avatars that make a member list read as people — start here.
async fn upload_media(
    client: &Client,
    hs: &str,
    token: &str,
    filename: &str,
    content_type: &str,
    bytes: Vec<u8>,
) -> anyhow::Result<String> {
    let resp: serde_json::Value = client
        .post(format!("{hs}/_matrix/media/v3/upload"))
        .bearer_auth(token)
        .query(&[("filename", filename)])
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .body(bytes)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    resp.get("content_uri")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("media upload response missing content_uri"))
}

/// Create a space — a room whose `m.room.create` carries `type: m.space`.
///
/// Name, topic and avatar are set afterwards through the timestamped state
/// route rather than here: room creation cannot be backdated (no `ts` on
/// `/createRoom`), and everything that *can* be should be, so a demo room's
/// history does not end in a block of today-dated setup.
async fn create_space(client: &Client, hs: &str, token: &str) -> anyhow::Result<String> {
    let resp: RoomResponse = client
        .post(format!("{hs}/_matrix/client/v3/createRoom"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "preset": "private_chat",
            "visibility": "private",
            "creation_content": { "type": "m.space" }
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp.room_id)
}

/// Link a room into a space, from both ends: `m.space.child` in the space and
/// `m.space.parent` in the child. Clients read the child list, but the parent
/// edge is what lets a room name the space it belongs to.
async fn add_space_child(
    client: &Client,
    hs: &str,
    actor: &str,
    space_id: &str,
    child_id: &str,
    ts: i64,
) -> anyhow::Result<()> {
    send_appservice_state(
        client,
        hs,
        space_id,
        actor,
        "m.space.child",
        child_id,
        serde_json::json!({ "via": [SERVER_NAME], "suggested": true }),
        ts,
    )
    .await?;
    send_appservice_state(
        client,
        hs,
        child_id,
        actor,
        "m.space.parent",
        space_id,
        serde_json::json!({ "via": [SERVER_NAME], "canonical": true }),
        ts,
    )
    .await
}

async fn set_room_name(
    client: &Client,
    hs: &str,
    room: &str,
    actor: &str,
    name: &str,
    ts: i64,
) -> anyhow::Result<()> {
    send_appservice_state(
        client,
        hs,
        room,
        actor,
        "m.room.name",
        "",
        serde_json::json!({ "name": name }),
        ts,
    )
    .await
}

async fn set_room_topic(
    client: &Client,
    hs: &str,
    room: &str,
    actor: &str,
    topic: &str,
    ts: i64,
) -> anyhow::Result<()> {
    send_appservice_state(
        client,
        hs,
        room,
        actor,
        "m.room.topic",
        "",
        serde_json::json!({ "topic": topic }),
        ts,
    )
    .await
}

async fn set_room_avatar(
    client: &Client,
    hs: &str,
    room: &str,
    actor: &str,
    mxc: &str,
    ts: i64,
) -> anyhow::Result<()> {
    send_appservice_state(
        client,
        hs,
        room,
        actor,
        "m.room.avatar",
        "",
        serde_json::json!({ "url": mxc }),
        ts,
    )
    .await
}

/// Write a membership as any local user, backdated.
///
/// Synapse routes `m.room.member` through its membership handler, which honours
/// the appservice `ts` (MSC3316) — verified against the stack, not assumed. It
/// matters: joins written at the current time would pile up a block of "joined"
/// rows at the *end* of every seeded room, after the story the room is telling.
async fn set_membership(
    client: &Client,
    hs: &str,
    room: &str,
    actor: &str,
    target: &str,
    content: serde_json::Value,
    ts: i64,
) -> anyhow::Result<()> {
    send_appservice_state(
        client,
        hs,
        room,
        actor,
        "m.room.member",
        target,
        content,
        ts,
    )
    .await
}

// Same shape as the send route it wraps; see `send_appservice_event`.
#[allow(clippy::too_many_arguments)]
async fn send_appservice_state(
    client: &Client,
    hs: &str,
    room: &str,
    actor: &str,
    event_type: &str,
    state_key: &str,
    content: serde_json::Value,
    ts: i64,
) -> anyhow::Result<()> {
    let ts = ts.to_string();
    client
        .put(format!(
            "{hs}/_matrix/client/v3/rooms/{}/state/{}/{}",
            url_path(room),
            url_path(event_type),
            url_path(state_key)
        ))
        .bearer_auth(AS_TOKEN)
        .query(&[("user_id", actor), ("ts", ts.as_str())])
        .json(&content)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// Create a direct chat: no name, no topic, and `is_direct` on the room. The
/// invite is written separately (backdated, carrying `is_direct` itself), and
/// the creator's `m.direct` account data is what finally makes it a DM — the
/// title both clients derive comes from the membership, but the room only
/// counts as direct because the account data says so.
async fn create_dm(client: &Client, hs: &str, creator_token: &str) -> anyhow::Result<String> {
    let resp: RoomResponse = client
        .post(format!("{hs}/_matrix/client/v3/createRoom"))
        .bearer_auth(creator_token)
        .json(&serde_json::json!({
            "preset": "trusted_private_chat",
            "visibility": "private",
            "is_direct": true
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp.room_id)
}

async fn set_direct_account_data(
    client: &Client,
    hs: &str,
    token: &str,
    user_id: &str,
    direct: &serde_json::Value,
) -> anyhow::Result<()> {
    client
        .put(format!(
            "{hs}/_matrix/client/v3/user/{}/account_data/m.direct",
            url_path(user_id)
        ))
        .bearer_auth(token)
        .json(direct)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// Set a user's global profile through the appservice, so their display name
/// and avatar are already right on the `m.room.member` event that joins them.
async fn set_profile(
    client: &Client,
    hs: &str,
    user_id: &str,
    display_name: &str,
    avatar_mxc: Option<&str>,
) -> anyhow::Result<()> {
    client
        .put(format!(
            "{hs}/_matrix/client/v3/profile/{}/displayname",
            url_path(user_id)
        ))
        .bearer_auth(AS_TOKEN)
        .query(&[("user_id", user_id)])
        .json(&serde_json::json!({ "displayname": display_name }))
        .send()
        .await?
        .error_for_status()?;
    if let Some(mxc) = avatar_mxc {
        client
            .put(format!(
                "{hs}/_matrix/client/v3/profile/{}/avatar_url",
                url_path(user_id)
            ))
            .bearer_auth(AS_TOKEN)
            .query(&[("user_id", user_id)])
            .json(&serde_json::json!({ "avatar_url": mxc }))
            .send()
            .await?
            .error_for_status()?;
    }
    Ok(())
}

fn resolve_axon_bin() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("AXON_SERVER_BIN") {
        return Ok(std::fs::canonicalize(PathBuf::from(path))?);
    }
    command_status(
        Command::new(env_cargo()).args(["build", "-p", "axon-server"]),
        "cargo build -p axon-server",
    )?;
    let bin = target_dir()?.join("debug").join(if cfg!(windows) {
        "axon-server.exe"
    } else {
        "axon-server"
    });
    if !bin.exists() {
        bail!("axon binary not found at {}", bin.display());
    }
    Ok(bin)
}

fn start_axon(
    bin: &Path,
    run_dir: &Path,
    log: &Path,
    database_url: &str,
    port: u16,
) -> anyhow::Result<u32> {
    let stdout = File::create(log).with_context(|| format!("create {}", log.display()))?;
    let stderr = stdout.try_clone().context("clone axon log file")?;
    let mut command = Command::new(bin);
    command
        .current_dir(run_dir)
        .env("DATABASE_URL", database_url)
        .env("AXON_SERVER__PORT", port.to_string())
        .env("AXON_SYNC__STORE_KEY", "smoke-local-stack-key")
        .env("AXON_SYNC__TIMELINE_LIMIT", AXON_TIMELINE_LIMIT)
        // Everything on disk goes under this run's directory. The defaults are
        // platform data/cache dirs shared by every axon on the machine, and
        // Tantivy takes an exclusive lock on its index — so a second stack
        // would fail to boot with "Failed to acquire index lock" rather than
        // with anything that names the real problem. That is easy to hit now
        // that a `--corpus` demo stack is meant to stay up for a while.
        .env("AXON_SYNC__DATA_DIR", run_dir.join("sync"))
        .env("AXON_SEARCH__INDEX_PATH", run_dir.join("search"))
        .env("AXON_MEDIA__CACHE_DIR", run_dir.join("media"))
        .env("AXON_MEDIA__UPLOADS_DIR", run_dir.join("uploads"))
        .env("RUST_LOG", "info,axon_sync=debug")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Keep a --keep-up stack usable after the helper exits, even when the
        // command runner cleans up the helper's original process group.
        unsafe {
            command.pre_exec(|| {
                if setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let child = command.spawn().context("spawn axon-server")?;
    Ok(child.id())
}

async fn wait_for_axon(client: &Client, base: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Ok(resp) = client
            .get(format!("{base}/healthz"))
            .timeout(HTTP_REQUEST_TIMEOUT)
            .send()
            .await
        {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        if Instant::now() > deadline {
            bail!("Axon did not become healthy");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn issue_axon_token(bin: &Path, run_dir: &Path, database_url: &str) -> anyhow::Result<String> {
    let output = Command::new(bin)
        .current_dir(run_dir)
        .env("DATABASE_URL", database_url)
        .args(["token", "issue", "--label", "smoke-local-stack"])
        .output()
        .context("axon token issue")?;
    if !output.status.success() {
        bail!(
            "axon token issue failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .last()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("token issue printed no token"))
}

async fn login_axon(
    client: &Client,
    base: &str,
    token: &str,
    account: &MatrixAccount,
    homeserver_url: &str,
) -> anyhow::Result<Uuid> {
    let env: AxonEnvelope<AxonAccount> = client
        .post(format!("{base}/v1/accounts/login"))
        // Login blocks on the account's first sync, which grows with the world
        // being synced — a demo corpus of seven rooms and their media is well
        // past the client-wide 10s timeout.
        .timeout(LOGIN_TIMEOUT)
        .bearer_auth(token)
        .json(&serde_json::json!({
            "username": account.user_id,
            "password": account.password,
            "homeserver_url": homeserver_url
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(env.data.account_id)
}

async fn wait_for_axon_room(
    client: &Client,
    base: &str,
    token: &str,
    room_id: &str,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let resp = client
            .get(format!("{base}/v1/rooms"))
            .bearer_auth(token)
            .send()
            .await;
        if let Ok(resp) = resp {
            if resp.status() == StatusCode::OK {
                let text = resp.text().await.unwrap_or_default();
                if text.contains(room_id) {
                    return Ok(());
                }
            }
        }
        if Instant::now() > deadline {
            bail!("Axon did not sync seeded room {room_id}");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn write_manifest(path: &Path, manifest: &Manifest) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(manifest)? + "\n")
        .with_context(|| format!("write {}", path.display()))
}

fn read_manifest(path: &Path) -> anyhow::Result<Manifest> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

fn read_cleanup_manifest(path: &Path) -> anyhow::Result<CleanupManifest> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

fn command_status(cmd: &mut Command, label: &str) -> anyhow::Result<()> {
    let status = cmd.status().with_context(|| format!("run {label}"))?;
    if !status.success() {
        bail!("{label} failed with {status}");
    }
    Ok(())
}

fn env_cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned())
}

fn http_client() -> anyhow::Result<Client> {
    Ok(Client::builder().timeout(HTTP_REQUEST_TIMEOUT).build()?)
}

fn target_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CARGO_TARGET_DIR") {
        return Ok(PathBuf::from(dir));
    }
    Ok(workspace_root()?.join("target"))
}

fn workspace_root() -> anyhow::Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("cannot derive workspace root from {}", manifest.display()))
}

fn url_path(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('!', "%21")
        .replace('$', "%24")
        .replace(':', "%3A")
        .replace('@', "%40")
        .replace('/', "%2F")
}

fn date_ms(date: &str) -> anyhow::Result<i64> {
    let date = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
    Ok(date
        .and_hms_opt(12, 0, 0)
        .ok_or_else(|| anyhow!("invalid fixture date {date}"))?
        .and_utc()
        .timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::CleanupManifest;

    #[test]
    fn cleanup_manifest_accepts_pre_fixture_schema() {
        let manifest = serde_json::json!({
            "run_id": "old-run",
            "axon_base_url": "http://127.0.0.1:1",
            "axon_bearer_token": "token",
            "homeserver_url": "http://127.0.0.1:2",
            "axon_account_id": "00000000-0000-0000-0000-000000000000",
            "accounts": {},
            "rooms": {},
            "paths": {
                "run_dir": "/tmp/axon-smoke-old",
                "axon_log": "/tmp/axon-smoke-old/axon.log",
                "compose_project": "axon-smoke-old",
                "database_name": "axon_smoke_old",
                "axon_pid": 12345,
                "postgres_port": 1,
                "synapse_port": 2,
                "axon_port": 3
            },
            "keep_up": true
        });

        let cleanup: CleanupManifest =
            serde_json::from_value(manifest).expect("old manifest should parse for cleanup");
        assert_eq!(cleanup.paths.compose_project, "axon-smoke-old");
        assert_eq!(cleanup.paths.database_name, "axon_smoke_old");
        assert_eq!(cleanup.paths.axon_pid, 12345);
    }
}
