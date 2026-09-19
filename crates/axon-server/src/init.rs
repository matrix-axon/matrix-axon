//! `axon init` — first-run configuration bootstrap (M13, ADR 0051).
//!
//! Generates a minimal, valid server config so a fresh operator does not have to
//! hand-write TOML or invent a `store_key`. It writes just the two things that
//! have no safe default — the Postgres URL and a generated `store_key` — and
//! leaves everything else to the built-in defaults (including the platform
//! data/cache dirs from ADR 0050). Accounts are provisioned later at runtime via
//! `POST /v1/accounts/login`, so init never asks for Matrix credentials.
//!
//! When the database is unreachable it can also offer to start a matching local
//! Postgres in Docker — the container's credentials are derived from the config's
//! database URL, so they always agree (unlike compose, which reads its own
//! `POSTGRES_*` env). This is opt-in (a prompt, or `--start-postgres`) and only
//! for a loopback host.
//!
//! Guardrail: every prompt is gated on an interactive terminal. Under
//! systemd/Docker/CI (`--non-interactive`, or no TTY) it runs from flags and
//! defaults and never blocks. An unattended script that allocates a pseudo-TTY
//! still looks interactive to the OS; pass `--non-interactive` for that case.
//! See ADR 0051.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use axon_core::Config;
use axon_store::Store;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, is_raw_mode_enabled};
use percent_encoding::percent_decode_str;
use sqlx_postgres::PgPoolOptions;
use tokio::process::Command;
use url::Url;

use crate::cli::InitArgs;

/// Docker container / volume / image used when `axon init` starts a local Postgres.
const PG_CONTAINER: &str = "axon-postgres";
const PG_VOLUME: &str = "axon-init-pgdata";
const PG_IMAGE: &str = "postgres:16";

/// The default Postgres URL — matches `axon.toml.example` and the dev launchers.
const DEFAULT_DATABASE_URL: &str = "postgres://axon:axon@127.0.0.1:5432/axon";

/// Upper bound on the first-run database reachability probe, so `init` can't hang
/// on an unreachable host (sqlx's own connect timeout is much longer).
const DB_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on each Docker CLI call. Docker is a process/network boundary:
/// a wedged daemon must not hang `axon init` forever.
const DOCKER_TIMEOUT: Duration = Duration::from_secs(10);

/// Run the `axon init` subcommand.
pub async fn run(args: &InitArgs, cli_config: Option<&Path>) -> anyhow::Result<()> {
    let interactive = !args.non_interactive && io::stdin().is_terminal();
    let explicit_target = explicit_write_target(cli_config);
    let explicit = explicit_target.is_some();
    let discovered = if explicit {
        None
    } else {
        Config::discover_config_path()?
    };
    let target = choose_write_target(
        explicit_target,
        discovered.clone(),
        Config::platform_config_path(),
        args.force,
    )?;

    // Refuse to clobber a config that already governs, unless --force: regenerating
    // `store_key` would orphan any data encrypted under the old one. For an explicit
    // target (`--config` / `$AXON_CONFIG`) only that file counts; for the default
    // platform target we also refuse if another config is already discoverable, so
    // init can't silently create a shadowed second config.
    if !args.force {
        let existing = if target.exists() {
            Some(target.clone())
        } else if !explicit {
            discovered
        } else {
            None
        };
        if let Some(path) = existing {
            if Config::is_legacy_platform_config(&path) {
                let current = Config::platform_config_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<platform config dir>/config.toml".to_string());
                bail!(
                    "a configuration already exists at {}. --force writes a new \
                     file at {current} with a new store_key (which cannot read \
                     data encrypted under the old key) and leaves this file in \
                     place, shadowed. Copy this file to the new path if you \
                     want to keep the existing key.",
                    path.display()
                );
            }
            bail!(
                "a configuration already exists at {}. Use --force to overwrite \
                 (this regenerates store_key, which orphans any existing encrypted data).",
                path.display()
            );
        }
    }

    let database_url = match &args.database_url {
        Some(url) => url.clone(),
        None if interactive => prompt_database_url()?,
        None => DEFAULT_DATABASE_URL.to_string(),
    };

    let store_key = generate_store_key();

    write_config(&target, &database_url, &store_key, args.force)
        .with_context(|| format!("writing config to {}", target.display()))?;
    println!("Wrote configuration to {}", target.display());

    probe_and_maybe_mint(args, interactive, &database_url).await?;

    println!(
        "\nNext steps:\n  • Ensure Postgres is running (see the URL above).\n  \
         • Start the server: `axon`\n  \
         • Add a Matrix account from a client (POST /v1/accounts/login)."
    );
    Ok(())
}

/// Bare `axon` found no configuration and is attached to a terminal: offer to run
/// first-time setup. Returns the freshly-loaded config on success, or `None` if
/// the operator declined (the caller then surfaces the original load error).
pub async fn offer_on_first_run(cli_config: Option<&Path>) -> anyhow::Result<Option<Config>> {
    println!("No configuration file found.");
    if !prompt_yes_no("Run first-time setup now?", true)? {
        return Ok(None);
    }
    let args = InitArgs {
        database_url: None,
        force: false,
        non_interactive: false,
        no_token: false,
        print_token: false,
        start_postgres: false,
        tui_config: false,
    };
    run(&args, cli_config).await?;
    let config =
        Config::load_from(cli_config).context("loading the just-generated configuration")?;
    Ok(Some(config))
}

/// Explicit config target, if any: `--config` > `$AXON_CONFIG`.
fn explicit_write_target(cli_config: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = cli_config {
        return Some(path.to_path_buf());
    }
    std::env::var_os("AXON_CONFIG").map(PathBuf::from)
}

/// Where to write the generated config. Without an explicit target, normal init
/// writes the platform config path; `--force` overwrites the config that would
/// actually govern boot when one already exists (e.g. `./axon.toml`), otherwise
/// it also uses the platform path.
///
/// A discovered *legacy* platform file (`~/.config/axon/axon.toml`) is never a
/// write target: it stays readable, and `--force` falls through to the current
/// platform path so init cannot clobber it (ADR 0050 amendment).
fn choose_write_target(
    explicit: Option<PathBuf>,
    discovered: Option<PathBuf>,
    platform: Option<PathBuf>,
    force: bool,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if force {
        if let Some(path) = discovered {
            if !Config::is_legacy_platform_config(&path) {
                return Ok(path);
            }
        }
    }
    platform.context(
        "cannot determine a platform config directory (no home directory found); \
         pass --config <PATH> to choose where to write the config",
    )
}

/// A fresh `store_key`: 256 bits of CSPRNG entropy, hex-encoded (matching the
/// `openssl rand -hex 32` form the dev launchers document).
fn generate_store_key() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Render and write the minimal config, `0600` on Unix. `toml_edit` handles value
/// escaping (a database URL can carry characters that need quoting).
fn write_config(
    target: &Path,
    database_url: &str,
    store_key: &str,
    overwrite: bool,
) -> anyhow::Result<()> {
    use toml_edit::{value, DocumentMut, Item, Table};

    let mut doc = DocumentMut::new();

    let mut database = Table::new();
    database["url"] = value(database_url);
    doc["database"] = Item::Table(database);
    if let Some(t) = doc["database"].as_table_mut() {
        t.decor_mut().set_prefix(
            "# Generated by `axon init` (ADR 0051). Everything not set here uses built-in\n\
             # defaults; see axon.toml.example for the full reference.\n\n\
             # Postgres connection URL (required).\n",
        );
    }

    let mut sync = Table::new();
    sync["store_key"] = value(store_key);
    doc["sync"] = Item::Table(sync);
    if let Some(t) = doc["sync"].as_table_mut() {
        t.decor_mut().set_prefix(
            "\n# Symmetric key: encrypts access tokens at rest and passphrases the SDK store.\n\
             # Generated once — changing it orphans existing encrypted data. Keep it secret.\n",
        );
    }

    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating config directory {}", parent.display()))?;
        }
    }

    write_secure(target, doc.to_string().as_bytes(), overwrite)?;
    Ok(())
}

/// Write `contents` to `target`, creating it `0600` and tightening perms on an
/// overwrite (`OpenOptions::mode` only applies on creation). The non-overwrite
/// path uses an atomic create so two concurrent `axon init` runs cannot both pass
/// the preflight guard and silently rotate `store_key`.
fn write_secure(target: &Path, contents: &[u8], overwrite: bool) -> io::Result<()> {
    use std::fs::OpenOptions;

    let mut opts = OpenOptions::new();
    opts.write(true);
    if overwrite {
        opts.create(true).truncate(true);
    } else {
        opts.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(target)?;
    file.write_all(contents)?;
    file.flush()?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    harden_windows_acl(target)?;
    Ok(())
}

#[cfg(windows)]
fn harden_windows_acl(target: &Path) -> io::Result<()> {
    let user = match (std::env::var_os("USERDOMAIN"), std::env::var_os("USERNAME")) {
        (Some(domain), Some(user)) if !domain.is_empty() && !user.is_empty() => {
            let mut account = domain;
            account.push("\\");
            account.push(user);
            account
        }
        (_, Some(user)) if !user.is_empty() => user,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "USERNAME is not set; cannot harden config ACL",
            ))
        }
    };
    let user_grant = format!("{}:F", user.to_string_lossy());
    let status = std::process::Command::new("icacls")
        .arg(target)
        .args(["/inheritance:r", "/grant:r"])
        .arg(user_grant)
        .args([
            "/grant:r",
            "*S-1-5-18:F", // Local System
            "/grant:r",
            "*S-1-5-32-544:F", // Administrators
        ])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Other,
            format!("icacls failed with status {status}"),
        ))
    }
}

/// Probe database reachability (bounded), report it, and mint a first token when
/// the DB is up and the operator wants one. A missing DB is not fatal — the
/// config is already written; init just tells the operator what to do next (and,
/// on request, starts a matching local Postgres in Docker).
async fn probe_and_maybe_mint(
    args: &InitArgs,
    interactive: bool,
    database_url: &str,
) -> anyhow::Result<()> {
    print!("Checking database connectivity… ");
    let _ = io::stdout().flush();

    let store = match connect_ready(database_url).await {
        Ok(store) => {
            println!("connected (schema ready).");
            Some(store)
        }
        Err(err) => {
            println!("not reachable ({err}).");
            maybe_start_postgres(args, interactive, database_url).await
        }
    };

    let Some(store) = store else {
        println!(
            "Start Postgres, then mint a client token with \
             `axon token issue --label <name>`.\n\n\
             For Axon's default Postgres docker configuration, \
             enter the following from within the Axon directory:\n\n\
             docker compose up -d postgres"
        );
        if args.print_token || args.tui_config {
            bail!("database is not reachable, so the requested client token could not be minted");
        }
        return Ok(());
    };

    // `--tui-config` / `--print-token` imply minting; otherwise ask (interactive)
    // or skip (non-interactive default).
    let want_token = if args.no_token {
        false
    } else if args.print_token || args.tui_config {
        true
    } else if interactive {
        prompt_yes_no("Mint a first client bearer token now?", true).unwrap_or(false)
    } else {
        false
    };

    if !want_token {
        println!("Mint a client token when ready with `axon token issue --label <name>`.");
        return Ok(());
    }

    match store.issue_token("axon init").await {
        Ok(issued) => {
            println!(
                "\nClient bearer token (copy it now — it will not be shown again):\n{}",
                issued.token
            );
            maybe_write_tui_config(args, interactive, &issued.token);
        }
        Err(err) => {
            if args.print_token || args.tui_config {
                return Err(err).context("minting the requested client token");
            }
            eprintln!("warning: config written, but minting a token failed: {err:#}");
            eprintln!("Retry later with `axon token issue --label <name>`.");
        }
    }
    Ok(())
}

/// Base URL an axon-tui config points at by default — Axon's loopback bind.
const TUI_BASE_URL: &str = "http://127.0.0.1:8080";

/// Optionally write the freshly-minted token into an axon-tui client config so
/// the TUI can connect with no manual step. Opt-in (prompt or `--tui-config`).
///
/// This is the one place `axon-server` reaches into the `clients/` silo's config
/// shape: it mirrors axon-tui's own path resolution and `[server]` schema
/// (`clients/tui/src/config.rs`) rather than the server's ADR 0050 platform dirs,
/// which differ. If axon-tui changes either, update this to match.
fn maybe_write_tui_config(args: &InitArgs, interactive: bool, token: &str) {
    let go = if args.tui_config {
        true
    } else if interactive {
        prompt_yes_no(
            "Also write an axon-tui client config with this token?",
            true,
        )
        .unwrap_or(false)
    } else {
        false
    };
    if !go {
        return;
    }

    let Some(path) = tui_config_path() else {
        eprintln!("Could not determine the axon-tui config location (no HOME); skipping.");
        return;
    };
    match write_tui_config(&path, token) {
        Ok(()) => println!("Wrote axon-tui config to {}", path.display()),
        Err(err) => eprintln!("warning: could not write axon-tui config: {err:#}"),
    }
}

/// axon-tui's config path, mirroring `clients/tui/src/config.rs::config_path`:
/// `$XDG_CONFIG_HOME/axon-tui/config.toml`, else `$HOME/.config/axon-tui/config.toml`.
fn tui_config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir).join("axon-tui").join("config.toml"));
    }
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("axon-tui")
            .join("config.toml"),
    )
}

/// Merge `[server].bearer_token` (and `base_url` if unset) into an axon-tui config,
/// preserving everything else; create a minimal one if absent. Written `0600`
/// since it now carries a bearer token.
fn write_tui_config(path: &Path, token: &str) -> anyhow::Result<()> {
    use toml_edit::{value, DocumentMut};

    let mut doc = if path.exists() {
        std::fs::read_to_string(path)?
            .parse::<DocumentMut>()
            .context("parsing existing axon-tui config")?
    } else {
        DocumentMut::new()
    };

    ensure_table(&mut doc, "server")?;
    let server = doc["server"]
        .as_table_mut()
        .expect("server table just ensured");
    // Respect an existing base_url (the user may point at a remote server); only
    // fill it when absent. Always (re)set the token — that's the point.
    if server.get("base_url").is_none() {
        server["base_url"] = value(TUI_BASE_URL);
    }
    server["bearer_token"] = value(token);

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    write_secure(path, doc.to_string().as_bytes(), true)?;
    Ok(())
}

fn ensure_table(doc: &mut toml_edit::DocumentMut, key: &str) -> anyhow::Result<()> {
    use toml_edit::{Item, Table, Value};

    match doc.get_mut(key) {
        Some(Item::Table(_)) => Ok(()),
        Some(Item::Value(Value::InlineTable(inline))) => {
            let mut table = Table::new();
            for (name, value) in inline.iter() {
                table.insert(name, Item::Value(value.clone()));
            }
            doc[key] = Item::Table(table);
            Ok(())
        }
        Some(_) => bail!("existing `{key}` entry must be a table"),
        None => {
            doc[key] = Item::Table(Table::new());
            Ok(())
        }
    }
}

/// Reach the database with a hard timeout, then run migrations without racing the
/// short reachability clock. This keeps slow-but-valid first-boot migrations from
/// being misreported as an unreachable database.
async fn connect_ready(url: &str) -> anyhow::Result<Store> {
    probe_database_reachable(url).await?;
    Store::connect(url, 2)
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))
}

/// Attempt a cheap Postgres connection with a hard timeout so an unreachable host
/// can't stall `init`. This does not run migrations.
async fn probe_database_reachable(url: &str) -> anyhow::Result<()> {
    let connect = PgPoolOptions::new().max_connections(1).connect(url);
    match tokio::time::timeout(DB_PROBE_TIMEOUT, connect).await {
        Ok(Ok(pool)) => {
            pool.close().await;
            Ok(())
        }
        Ok(Err(err)) => Err(anyhow::anyhow!(err.to_string())),
        Err(_) => bail!("timed out after {}s", DB_PROBE_TIMEOUT.as_secs()),
    }
}

/// Local Postgres connection parameters parsed from the config's database URL,
/// used to launch a container whose credentials match the config exactly.
struct LocalPg {
    user: String,
    password: String,
    db: String,
    port: u16,
}

/// On an unreachable database, optionally start a matching local Postgres in
/// Docker and wait for it to come up. The container's credentials are derived
/// from `database_url`, so they always agree with the config just written (unlike
/// the compose path, which reads `POSTGRES_*` from the environment). Returns a
/// connected store when Postgres becomes ready, else `None`.
async fn maybe_start_postgres(
    args: &InitArgs,
    interactive: bool,
    database_url: &str,
) -> Option<Store> {
    let go = if args.start_postgres {
        true
    } else if interactive {
        prompt_yes_no("Start a local Postgres in Docker now?", true).unwrap_or(false)
    } else {
        false
    };
    if !go {
        return None;
    }

    // Only a local, password-bearing URL can be served by a container we run.
    let pg = parse_local_pg(database_url)?;

    if !docker_available().await {
        println!(
            "Docker was not detected (or its daemon isn't running), so Axon can't start \
             Postgres for you. Install/start Docker, or start Postgres yourself."
        );
        return None;
    }

    if let Err(err) = start_pg_container(&pg).await {
        eprintln!("Could not start Postgres in Docker: {err:#}");
        return None;
    }

    print!("Waiting for Postgres to become ready");
    let _ = io::stdout().flush();
    match wait_until_ready(database_url).await {
        Ok(store) => {
            println!(" ready (schema ready).");
            Some(store)
        }
        Err(err) => {
            println!(" not ready ({err}).");
            None
        }
    }
}

/// Parse a `postgres://…` URL into container parameters, but only when Axon could
/// actually serve it locally: a loopback host and a non-empty password (the
/// `postgres` image refuses to initialize without one). Prints why and returns
/// `None` otherwise.
fn parse_local_pg(database_url: &str) -> Option<LocalPg> {
    let url = match Url::parse(database_url) {
        Ok(url) => url,
        Err(err) => {
            println!("The database URL is not valid ({err}); Axon won't start a container for it.");
            return None;
        }
    };
    if !matches!(url.scheme(), "postgres" | "postgresql") {
        println!(
            "The database URL scheme ({}) is not postgres/postgresql; Axon won't start a container for it.",
            url.scheme()
        );
        return None;
    }
    let host = url.host_str().unwrap_or_default();
    if !matches!(host, "localhost" | "127.0.0.1" | "::1") {
        println!(
            "The database host ({host}) is not local, so Axon won't start a container for it."
        );
        return None;
    }
    let password = url.password().unwrap_or_default();
    if password.is_empty() {
        println!(
            "The database URL has no password; Axon can't start a Postgres container without one."
        );
        return None;
    }
    let user = match percent_decode_component("username", url.username())? {
        decoded if decoded.is_empty() => "axon".to_owned(),
        decoded => decoded,
    };
    let db = match percent_decode_component("database name", url.path().trim_start_matches('/'))? {
        decoded if decoded.is_empty() => "axon".to_owned(),
        decoded => decoded,
    };
    Some(LocalPg {
        user,
        password: percent_decode_component("password", password)?,
        db,
        port: url.port().unwrap_or(5432),
    })
}

fn percent_decode_component(label: &str, component: &str) -> Option<String> {
    match percent_decode_str(component).decode_utf8() {
        Ok(decoded) => Some(decoded.into_owned()),
        Err(err) => {
            println!(
                "The database URL {label} has invalid percent-encoding/UTF-8 ({err}); \
                 Axon won't start a container for it."
            );
            None
        }
    }
}

/// True when the Docker CLI is present *and* its daemon is reachable (`docker
/// info` succeeds only when both hold).
async fn docker_available() -> bool {
    docker_output(&["info"], "`docker info`")
        .await
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Start the local Postgres container: reuse an existing `axon-postgres` (just
/// start it) or create a fresh one bound to loopback with a persistent volume.
async fn start_pg_container(pg: &LocalPg) -> anyhow::Result<()> {
    if container_exists(PG_CONTAINER).await? {
        validate_container_env(PG_CONTAINER, pg).await?;
        validate_container_port(PG_CONTAINER, pg).await?;
        println!("Starting existing Docker container `{PG_CONTAINER}`…");
        run_docker(&["start", PG_CONTAINER]).await
    } else {
        if volume_exists(PG_VOLUME).await? {
            bail!(
                "Docker volume `{PG_VOLUME}` already exists but container `{PG_CONTAINER}` does \
                 not. Axon cannot verify whether that existing data directory matches the \
                 requested database URL credentials; recreate the container with matching \
                 credentials, remove the volume, or use a different Postgres URL."
            );
        }
        println!("Creating Docker container `{PG_CONTAINER}` ({PG_IMAGE})…");
        // Bind only to loopback — the container is for this host, not the network.
        let publish = format!("127.0.0.1:{}:5432", pg.port);
        let volume = format!("{PG_VOLUME}:/var/lib/postgresql/data");
        let env_user = format!("POSTGRES_USER={}", pg.user);
        let env_pass = format!("POSTGRES_PASSWORD={}", pg.password);
        let env_db = format!("POSTGRES_DB={}", pg.db);
        run_docker(&[
            "run",
            "-d",
            "--name",
            PG_CONTAINER,
            "-e",
            &env_user,
            "-e",
            &env_pass,
            "-e",
            &env_db,
            "-p",
            &publish,
            "-v",
            &volume,
            PG_IMAGE,
        ])
        .await
    }
}

/// Whether a container with exactly `name` exists (running or stopped).
async fn container_exists(name: &str) -> anyhow::Result<bool> {
    let out = docker_output(
        &["ps", "-aq", "-f", &format!("name=^{name}$")],
        "`docker ps`",
    )
    .await?;
    Ok(!out.stdout.is_empty())
}

/// Whether a Docker volume with exactly `name` exists.
async fn volume_exists(name: &str) -> anyhow::Result<bool> {
    let out = docker_output(
        &["volume", "ls", "-q", "-f", &format!("name=^{name}$")],
        "`docker volume ls`",
    )
    .await?;
    Ok(!out.stdout.is_empty())
}

async fn validate_container_env(name: &str, pg: &LocalPg) -> anyhow::Result<()> {
    let out = docker_output(
        &[
            "inspect",
            "--format",
            "{{range .Config.Env}}{{println .}}{{end}}",
            name,
        ],
        "`docker inspect`",
    )
    .await?;
    if !out.status.success() {
        bail!(
            "`docker inspect {name}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let env: Vec<&str> = stdout.lines().collect();
    let existing_user = docker_env_value(&env, "POSTGRES_USER");
    let existing_password = docker_env_value(&env, "POSTGRES_PASSWORD");
    let existing_db = docker_env_value(&env, "POSTGRES_DB");
    if existing_user != Some(pg.user.as_str())
        || existing_password != Some(pg.password.as_str())
        || existing_db != Some(pg.db.as_str())
    {
        bail!(
            "existing Docker container `{name}` was initialized with different Postgres \
             credentials than the requested database URL. Postgres ignores changed \
             POSTGRES_* variables after initdb; use a matching --database-url, remove the \
             container and volume, or start Postgres yourself."
        );
    }
    Ok(())
}

async fn validate_container_port(name: &str, pg: &LocalPg) -> anyhow::Result<()> {
    let out = docker_output(&["port", name, "5432/tcp"], "`docker port`").await?;
    if !out.status.success() {
        bail!(
            "`docker port {name} 5432/tcp` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let expected = pg.port.to_string();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let matches_port = stdout.lines().any(|line| {
        line.trim()
            .rsplit_once(':')
            .is_some_and(|(_, port)| port == expected)
    });
    if !matches_port {
        bail!(
            "existing Docker container `{name}` is not bound to requested Postgres port {}. \
             Use the port from the existing container, remove it, or start Postgres yourself.",
            pg.port
        );
    }
    Ok(())
}

fn docker_env_value<'a>(env: &'a [&str], key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    env.iter()
        .find_map(|value| value.strip_prefix(prefix.as_str()))
}

/// Run `docker <args>`, turning a non-zero exit into an error carrying stderr.
async fn run_docker(args: &[&str]) -> anyhow::Result<()> {
    let out = docker_output(args, "docker").await?;
    if !out.status.success() {
        bail!(
            "`docker {}` failed: {}",
            redact_docker_args(args),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

async fn docker_output(args: &[&str], description: &str) -> anyhow::Result<std::process::Output> {
    let mut command = Command::new("docker");
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = command.output();
    match tokio::time::timeout(DOCKER_TIMEOUT, output).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(err)) => Err(err).with_context(|| format!("running {description}")),
        Err(_) => bail!(
            "{description} timed out after {}s",
            DOCKER_TIMEOUT.as_secs()
        ),
    }
}

fn redact_docker_args(args: &[&str]) -> String {
    args.iter()
        .map(|arg| {
            if arg.starts_with("POSTGRES_USER=") {
                "POSTGRES_USER=<redacted>"
            } else if arg.starts_with("POSTGRES_PASSWORD=") {
                "POSTGRES_PASSWORD=<redacted>"
            } else if arg.starts_with("POSTGRES_DB=") {
                "POSTGRES_DB=<redacted>"
            } else {
                arg
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Poll the database until it accepts a connection or ~30s elapses. The first
/// successful reachability probe then runs migrations once, leaving the schema
/// ready.
async fn wait_until_ready(database_url: &str) -> anyhow::Result<Store> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if probe_database_reachable(database_url).await.is_ok() {
            return Store::connect(database_url, 2)
                .await
                .map_err(|err| anyhow::anyhow!(err.to_string()));
        }
        if Instant::now() >= deadline {
            bail!("timed out after 30s");
        }
        print!(".");
        let _ = io::stdout().flush();
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn prompt_database_url() -> anyhow::Result<String> {
    let line = prompt_line(&format!("Postgres URL [{DEFAULT_DATABASE_URL}]: "))?;
    let trimmed = line.trim();
    Ok(if trimmed.is_empty() {
        DEFAULT_DATABASE_URL.to_string()
    } else {
        trimmed.to_string()
    })
}

pub(crate) fn prompt_yes_no(question: &str, default_yes: bool) -> anyhow::Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    let line = prompt_line(&format!("{question} {hint} "))?;
    Ok(match line.trim().to_ascii_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default_yes,
    })
}

fn prompt_line(prompt: &str) -> anyhow::Result<String> {
    let _raw_mode = suspend_raw_mode_for_prompt()?;
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line)
}

fn suspend_raw_mode_for_prompt() -> anyhow::Result<RawModeGuard> {
    let was_enabled = is_raw_mode_enabled()?;
    if was_enabled {
        disable_raw_mode()?;
    }
    Ok(RawModeGuard {
        restore: was_enabled,
    })
}

struct RawModeGuard {
    restore: bool,
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.restore {
            let _ = enable_raw_mode();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_key_is_64_hex_chars_and_unique() {
        let a = generate_store_key();
        let b = generate_store_key();
        assert_eq!(a.len(), 64, "256 bits hex-encoded");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "each call is fresh CSPRNG");
    }

    #[test]
    fn init_volume_does_not_collide_with_compose_volume() {
        assert_ne!(PG_VOLUME, "axon-pgdata");
    }

    #[test]
    fn force_without_explicit_target_overwrites_discovered_config() {
        let target = choose_write_target(
            None,
            Some(PathBuf::from("axon.toml")),
            Some(PathBuf::from("/platform/axon.toml")),
            true,
        )
        .expect("target");
        assert_eq!(target, PathBuf::from("axon.toml"));
    }

    #[test]
    fn non_force_without_explicit_target_uses_platform_write_target() {
        let target = choose_write_target(
            None,
            Some(PathBuf::from("axon.toml")),
            Some(PathBuf::from("/platform/axon.toml")),
            false,
        )
        .expect("target");
        assert_eq!(target, PathBuf::from("/platform/axon.toml"));
    }

    #[test]
    fn force_does_not_overwrite_legacy_platform_config() {
        let Some(legacy) = Config::legacy_platform_config_path() else {
            return;
        };
        let Some(platform) = Config::platform_config_path() else {
            return;
        };
        let target = choose_write_target(None, Some(legacy.clone()), Some(platform.clone()), true)
            .expect("target");
        assert_eq!(target, platform);
        assert_ne!(target, legacy);
    }

    #[test]
    fn explicit_target_wins_even_with_force_and_discovered_config() {
        let target = choose_write_target(
            Some(PathBuf::from("explicit.toml")),
            Some(PathBuf::from("axon.toml")),
            Some(PathBuf::from("/platform/axon.toml")),
            true,
        )
        .expect("target");
        assert_eq!(target, PathBuf::from("explicit.toml"));
    }

    #[test]
    fn written_config_is_valid_toml_with_the_two_keys() {
        let dir = std::env::temp_dir().join(format!("axon-init-{}", generate_store_key()));
        let target = dir.join("axon.toml");
        write_config(&target, "postgres://u:p@h:5432/db", "abc123", false).expect("write");

        let content = std::fs::read_to_string(&target).expect("read back");
        let doc: toml_edit::DocumentMut = content.parse().expect("valid toml");
        assert_eq!(
            doc["database"]["url"].as_str(),
            Some("postgres://u:p@h:5432/db")
        );
        assert_eq!(doc["sync"]["store_key"].as_str(), Some("abc123"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn database_url_with_special_chars_is_escaped_and_roundtrips() {
        // A password with a quote/backslash is why we render via toml_edit rather
        // than a format! template — it must survive a parse unchanged.
        let dir = std::env::temp_dir().join(format!("axon-init-{}", generate_store_key()));
        let target = dir.join("axon.toml");
        let url = r#"postgres://u:pa"ss\x@h/db"#;
        write_config(&target, url, "k", false).expect("write");

        let content = std::fs::read_to_string(&target).expect("read back");
        let doc: toml_edit::DocumentMut = content.parse().expect("valid toml");
        assert_eq!(doc["database"]["url"].as_str(), Some(url));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn written_config_is_owner_only_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("axon-init-{}", generate_store_key()));
        let target = dir.join("axon.toml");
        write_config(&target, "postgres://u:p@h/db", "k", false).expect("write");

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config holds a secret; must be owner-only");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tui_config_created_minimal_when_absent() {
        let dir = std::env::temp_dir().join(format!("axon-tui-{}", generate_store_key()));
        let path = dir.join("config.toml");
        write_tui_config(&path, "axon_TESTTOKEN").expect("write");

        let doc: toml_edit::DocumentMut = std::fs::read_to_string(&path)
            .unwrap()
            .parse()
            .expect("valid toml");
        assert_eq!(doc["server"]["base_url"].as_str(), Some(TUI_BASE_URL));
        assert_eq!(
            doc["server"]["bearer_token"].as_str(),
            Some("axon_TESTTOKEN")
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tui_config_merge_preserves_other_sections_and_existing_base_url() {
        let dir = std::env::temp_dir().join(format!("axon-tui-{}", generate_store_key()));
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            "[shortcuts]\nquit = \"ctrl-c\"\n\n[server]\nbase_url = \"https://remote:9000\"\n",
        )
        .unwrap();

        write_tui_config(&path, "axon_NEW").expect("merge");

        let doc: toml_edit::DocumentMut = std::fs::read_to_string(&path)
            .unwrap()
            .parse()
            .expect("valid toml");
        // User's other settings preserved.
        assert_eq!(doc["shortcuts"]["quit"].as_str(), Some("ctrl-c"));
        // Existing base_url respected (not clobbered); token set.
        assert_eq!(
            doc["server"]["base_url"].as_str(),
            Some("https://remote:9000")
        );
        assert_eq!(doc["server"]["bearer_token"].as_str(), Some("axon_NEW"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_config_refuses_existing_target_without_force() {
        let dir = std::env::temp_dir().join(format!("axon-init-{}", generate_store_key()));
        let target = dir.join("axon.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&target, "existing").unwrap();

        let err = write_config(&target, "postgres://u:p@h/db", "k", false).expect_err("refuse");
        assert_eq!(
            err.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::AlreadyExists)
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "existing");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_config_overwrites_existing_target_with_force() {
        let dir = std::env::temp_dir().join(format!("axon-init-{}", generate_store_key()));
        let target = dir.join("axon.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&target, "existing").unwrap();

        write_config(&target, "postgres://u:p@h/db", "k", true).expect("force write");
        let content = std::fs::read_to_string(&target).unwrap();
        assert!(content.contains("[database]"));
        assert_ne!(content, "existing");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tui_config_merge_preserves_inline_server_table_base_url() {
        let dir = std::env::temp_dir().join(format!("axon-tui-{}", generate_store_key()));
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            "server = { base_url = \"https://remote:9000\" }\n[shortcuts]\nquit = \"ctrl-c\"\n",
        )
        .unwrap();

        write_tui_config(&path, "axon_INLINE").expect("merge");

        let doc: toml_edit::DocumentMut = std::fs::read_to_string(&path)
            .unwrap()
            .parse()
            .expect("valid toml");
        assert_eq!(
            doc["server"]["base_url"].as_str(),
            Some("https://remote:9000")
        );
        assert_eq!(doc["server"]["bearer_token"].as_str(), Some("axon_INLINE"));
        assert_eq!(doc["shortcuts"]["quit"].as_str(), Some("ctrl-c"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn local_pg_decodes_url_credentials_like_sqlx() {
        let pg = parse_local_pg("postgres://ax%6Fn:p%40ss@127.0.0.1:5544/db%2Dname")
            .expect("local postgres url");
        assert_eq!(pg.user, "axon");
        assert_eq!(pg.password, "p@ss");
        assert_eq!(pg.db, "db-name");
        assert_eq!(pg.port, 5544);
    }

    #[test]
    fn local_pg_rejects_invalid_percent_encoded_password() {
        assert!(parse_local_pg("postgres://axon:p%FF@127.0.0.1:5432/axon").is_none());
    }

    #[test]
    fn docker_args_redact_postgres_env_values() {
        let args = [
            "run",
            "-e",
            "POSTGRES_USER=axon",
            "-e",
            "POSTGRES_PASSWORD=secret",
            "-e",
            "POSTGRES_DB=axon",
        ];
        let rendered = redact_docker_args(&args);
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("POSTGRES_USER=axon"));
        assert!(rendered.contains("POSTGRES_PASSWORD=<redacted>"));
    }
}
