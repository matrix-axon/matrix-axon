//! Axon binary — wires all crates together and owns the process.
//!
//! With no subcommand it runs the server. Boot sequence: load config, initialize
//! tracing, connect the store (running migrations), build the router, then serve
//! until a shutdown signal arrives. The `token` subcommand (M7b) is a short-lived
//! DB-only path for managing client bearer tokens — see [`token`]. The `init`
//! subcommand (M13) generates a starter config on first run — see [`init`].
//! `anyhow` is used here at the binary boundary; library crates use `thiserror`.

mod cli;
#[cfg(feature = "dev-tools")]
mod db;
mod devices;
mod gateway;
mod init;
mod lifecycle;
mod matrix_oauth_acquire;
mod media;
mod member_profiles;
mod oauth;
mod search;
mod status;
mod token;
mod trust;
mod uploads;
mod utd;
mod verification;

use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axon_core::Config;
use axon_store::Store;
use axon_sync::SyncEngine;
use clap::Parser;
use rand::Rng;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::cli::{Cli, Command};
use crate::devices::DeviceAdapter;
use crate::gateway::GatewayAdapter;
use crate::lifecycle::LifecycleAdapter;
use crate::matrix_oauth_acquire::MatrixOAuthAcquireAdapter;
use crate::media::CachingMediaProxy;
use crate::member_profiles::MemberProfileAdapter;
use crate::trust::TrustAdapter;
use crate::uploads::FilesystemStagedUploads;
use crate::verification::VerificationAdapter;

const SEARCH_SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const BOOTSTRAP_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env if present. Silently skipped when the file doesn't exist so
    // container / CI deployments that rely purely on environment variables
    // continue to work unchanged.
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    // `init` generates config, and `utd` is an API client for a running server;
    // both must run before this process tries to load its own server config.
    if let Some(command) = &cli.command {
        match command {
            Command::Init(args) => return init::run(args, cli.config.as_deref()).await,
            Command::Utd { action } => return utd::run(action).await,
            _ => {}
        }
    }

    // Config first, so we know how to configure logging. A `--config` flag (if
    // given) takes precedence over `AXON_CONFIG` / convention discovery.
    let config = match Config::load_from(cli.config.as_deref()) {
        Ok(config) => config,
        Err(err) => {
            // First-run sugar: bare `axon` with no configuration anywhere, on an
            // interactive terminal, offers to generate one (delegating to the same
            // `init` routine). Every other case — a subcommand, an explicit
            // `--config`, a config file present but broken, or no TTY — fails fast,
            // and never blocks a headless boot on a prompt (ADR 0051).
            let discovered = match Config::discover_config_path() {
                Ok(path) => path,
                Err(_) => return Err(err).context("loading configuration"),
            };
            let first_run = cli.command.is_none()
                && cli.config.is_none()
                && discovered.is_none()
                && !config_env_present()
                && std::io::stdin().is_terminal();
            if first_run {
                match init::offer_on_first_run(cli.config.as_deref()).await? {
                    Some(config) => config,
                    None => return Err(err).context("loading configuration"),
                }
            } else if discovered.is_none() && cli.config.is_none() && !config_env_present() {
                return Err(err).context(
                    "no configuration found. Run `axon init` to create one, or set the database \
                     URL via DATABASE_URL / AXON_DATABASE__URL. See axon.toml.example for the \
                     full reference.",
                );
            } else {
                return Err(err).context("loading configuration");
            }
        }
    };

    init_tracing(&config.log.level);

    match cli.command {
        #[cfg(feature = "dev-tools")]
        Some(Command::Db { action }) => db::run(action, &config).await,
        Some(Command::Token { action }) => token::run(action, &config).await,
        Some(Command::Search { action }) => search::run(action, &config),
        Some(Command::Utd { .. }) => unreachable!("utd runs before config load"),
        Some(Command::Oauth { action }) => oauth::run(action, &config).await,
        // Handled above, before config load.
        Some(Command::Init(_)) => unreachable!("init runs before config load"),
        None => serve(config).await,
    }
}

/// Run the long-lived HTTP/WebSocket server until a shutdown signal arrives.
async fn serve(config: Config) -> anyhow::Result<()> {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        git_hash = env!("AXON_GIT_HASH"),
        profile = env!("AXON_PROFILE"),
        build_time = env!("AXON_BUILD_TIME"),
        rustc_version = env!("AXON_RUSTC_VERSION"),
        "axon starting"
    );
    log_fd_limit();

    // Fail fast (before any side effects) on an unsafe bind. Axon serves plain
    // HTTP and the /v1 API carries credentials (login passwords, recovery keys,
    // bearer tokens); the tech spec requires client↔Axon TLS. So a non-loopback
    // bind is refused unless the operator explicitly accepts cleartext on the
    // wire — the safe setup is loopback + a TLS-terminating reverse proxy.
    let addr = config.socket_addr();
    if !addr.ip().is_loopback() {
        anyhow::ensure!(
            config.server.allow_insecure_bind,
            "refusing to bind non-loopback address {addr} over plain HTTP: Axon serves \
             credentials in cleartext. Front it with a TLS-terminating reverse proxy and bind \
             loopback, or set server.allow_insecure_bind = true \
             (AXON_SERVER__ALLOW_INSECURE_BIND=true) to override on a trusted network.",
        );
        tracing::warn!(
            %addr,
            "binding a non-loopback address over plain HTTP (server.allow_insecure_bind set); \
             ensure a TLS reverse proxy or trusted private network fronts Axon"
        );
    }

    let store = Store::connect(&config.database.url, config.database.max_connections)
        .await
        .context("connecting to database")?;
    let bootstrap = maybe_offer_web_bootstrap(&store, &config).await?;

    // Open the search index and spawn its background indexing actor (M9), when
    // enabled. The actor owns the sole Tantivy writer. `open` reports whether the
    // physical index is `fresh` (new, empty, or built against a different schema
    // version); if so the actor seeds the corpus from the store as a background
    // task — boot never blocks on it, even for a large corpus. Either way it then
    // drains the durable `search_outbox` change log, indexing live events as they
    // arrive. We hand its producer handle to the sync engine. We also retain the
    // read side (`SearchIndex`) and adapt it onto the API's `SearchQuery` port so
    // `GET /v1/search` can query it (M9b); the actor's writer and our retained Arc
    // both keep the underlying index alive.
    let (index_handle, index_join, index_cancel, search_port) = if config.search.enabled {
        let (search_index, fresh) = axon_search::SearchIndex::open(&config.search.index_path)
            .context("opening search index")?;
        let search_index = Arc::new(search_index);
        let handles = search_index
            .spawn_indexer(store.clone(), fresh, indexer_options(&config.search))
            .context("starting search indexer")?;
        let search_port: Arc<dyn axon_api::SearchQuery> = Arc::new(search::SearchAdapter::new(
            search_index,
            config.search.max_concurrent_queries,
            std::time::Duration::from_millis(config.search.query_timeout_ms),
        ));
        (
            Some(handles.handle),
            Some(handles.join),
            Some(handles.cancel),
            Some(search_port),
        )
    } else {
        tracing::info!("search disabled (search.enabled = false); not indexing");
        (None, None, None, None)
    };

    // Open the bounded on-disk media cache (M11). Its handle is threaded into
    // the sync engine so account-deletion teardown can purge an account's cached
    // media (ADR 0024 step 5). Opening it rebuilds the LRU index from disk and
    // evicts down to the configured cap; failure is fatal since the media route
    // depends on it.
    let media_cache = axon_media::MediaCache::open(&config.media)
        .await
        .context("opening media cache")?;
    spawn_fd_diagnostics(media_cache.clone());

    // Start the sync engine: runs one supervised Simplified Sliding Sync task
    // per active account (accounts come into existence only via the runtime
    // login/import API — there is no boot-time provisioning, ADR 0024).
    let sync_engine = SyncEngine::start(
        store.clone(),
        config.sync.clone(),
        index_handle,
        media_cache.handle(),
    )
    .await
    .context("starting sync engine")?;

    // The API shares the sync engine's live-event bus so `/v1/ws` can fan out
    // events as they're persisted, its message gateway (adapted onto the API's
    // MessageSender port) so the mutation routes can send via the SDK, its
    // lifecycle engine (adapted onto the AccountLifecycle port) so the login route
    // can add/reactivate accounts at runtime, and its verification engine (adapted
    // onto the VerificationService port) so the verify routes can drive SAS flows,
    // and its sender-trust engine (adapted onto the SenderTrustService port) so the
    // verification-bundle route can read per-event trust (M7c), and its
    // device-list engine (adapted onto the DeviceListService port) so a client
    // can list a user's devices before starting SAS verification (M16, ADR
    // 0060), and its cached member-profile engine so `/members` can enrich
    // missing avatar URLs without client-side fan-out. The bearer-token verifier
    // (M7b) is backed straight by the store.
    let sender = Arc::new(GatewayAdapter::new(
        sync_engine.gateway(),
        std::time::Duration::from_secs(config.sync.send_mutation_timeout_secs),
        std::time::Duration::from_secs(config.media.upstream_upload_timeout_secs),
        std::time::Duration::from_secs(config.sync.ephemeral_send_timeout_secs),
        std::time::Duration::from_secs(config.sync.membership_mutation_timeout_secs),
        std::time::Duration::from_secs(config.sync.room_entry_timeout_secs),
    ));
    // Same adapter, unsized onto the ephemeral-outbound port (read receipts /
    // typing notices, ADR 0067 / ADR 0068 M19a) alongside `MessageSender` below.
    let ephemeral: Arc<dyn axon_api::EphemeralSender> = sender.clone();
    // Same adapter again, unsized onto the room-membership port (leave/forget/
    // invite/kick/ban/unban, ADR 0068 M19b).
    let membership: Arc<dyn axon_api::MembershipSender> = sender.clone();
    // Same adapter again, unsized onto the room-entry port (join/knock/
    // create_room/create_dm, ADR 0068 M19c).
    let room_entry: Arc<dyn axon_api::RoomEntrySender> = sender.clone();
    // Same adapter again, unsized onto the room-settings port (name/topic/
    // avatar/tags, ADR 0068 M19d).
    let room_settings: Arc<dyn axon_api::RoomSettingsSender> = sender.clone();
    // Same adapter again, unsized onto the power-levels port (role
    // thresholds + per-user levels, ADR 0068 M19e).
    let power_levels: Arc<dyn axon_api::PowerLevelsSender> = sender.clone();
    // Same adapter again, unsized onto the account-actions port (profile,
    // ignore list, directory search, ADR 0068 M19f).
    let account_actions: Arc<dyn axon_api::AccountActionsSender> = sender.clone();
    let lifecycle = Arc::new(LifecycleAdapter(sync_engine.lifecycle()));
    let matrix_oauth_acquire = Arc::new(MatrixOAuthAcquireAdapter(
        sync_engine.matrix_oauth_acquire(),
    ));
    let verify = Arc::new(VerificationAdapter(sync_engine.verification()));
    let trust = Arc::new(TrustAdapter(sync_engine.sender_trust()));
    let devices = Arc::new(DeviceAdapter(sync_engine.devices()));
    let member_profiles = Arc::new(MemberProfileAdapter(sync_engine.member_profiles()));
    let verifier = Arc::new(axon_api::StoreTokenVerifier::new(store.clone()));
    let media = Arc::new(CachingMediaProxy::new(
        media_cache,
        sync_engine.media_fetcher(std::time::Duration::from_secs(
            config.media.fetch_timeout_secs,
        )),
    ));
    let uploads = Arc::new(
        FilesystemStagedUploads::new(store.clone(), &config.media)
            .context("configuring staged media uploads")?,
    );
    // M15c reconcile (ADR 0059, GH #286): resets crash-wedged `sending` rows,
    // prunes row-less staged-upload files, and sweeps already-expired staged
    // rows, all before any client traffic can race it. `spawn_expiry_sweeper`
    // then repeats the expiry sweep for the rest of the process's life.
    uploads.reconcile_boot().await;
    crate::uploads::spawn_expiry_sweeper(uploads.clone());
    let backfill_status = Arc::new(status::BackfillStatusAdapter(sync_engine.backfill_health()));
    let sync_status = Arc::new(status::SyncStatusAdapter(sync_engine.sync_health()));
    let sync_state = Arc::new(status::SyncStateAdapter(sync_engine.sync_health()));
    let backup_state = Arc::new(status::BackupStateAdapter(sync_engine.backup_health()));

    // OAuth 2.0 authorization server (M14, ADR 0054), when enabled. Provider
    // construction is async (discovery-doc fetch), so it happens here rather
    // than inside `AppState`/`router` — a failure here is fatal at boot,
    // consistent with the search index / media cache above.
    let oauth = if config.oauth.enabled {
        let runtime = build_oauth_runtime(&config.oauth)
            .await
            .context("configuring oauth")?;
        axon_api::spawn_oauth_rate_limit_sweeper(runtime.clone());
        Some(runtime)
    } else {
        tracing::info!("oauth disabled (oauth.enabled = false); /v1/oauth/* will 404");
        None
    };

    let mut state = axon_api::AppState::new(
        store,
        sync_engine.live_events(),
        sender,
        lifecycle,
        verify,
        trust,
        devices,
        verifier,
        media,
        search_port,
    )
    .with_backfill_status(backfill_status)
    .with_sync_status(sync_status)
    .with_sync_state(sync_state)
    .with_backup_state(backup_state)
    .with_matrix_oauth_acquire(matrix_oauth_acquire)
    .with_build_info(axon_api::BuildInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        git_hash: env!("AXON_GIT_HASH").to_owned(),
        profile: env!("AXON_PROFILE").to_owned(),
        build_time: env!("AXON_BUILD_TIME").to_owned(),
        rustc_version: env!("AXON_RUSTC_VERSION").to_owned(),
    })
    .with_member_profiles(member_profiles)
    .with_staged_uploads(uploads)
    .with_ephemeral(ephemeral)
    .with_membership(membership)
    .with_room_entry(room_entry)
    .with_room_settings(room_settings)
    .with_power_levels(power_levels)
    .with_account_actions(account_actions);
    if let Some(oauth) = oauth {
        state = state.with_oauth(oauth);
    }
    if let Some(bootstrap) = bootstrap.clone() {
        state = state.with_bootstrap(bootstrap);
    }
    let app = axon_api::router(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    tracing::info!(%addr, "axon listening");
    if let Some(bootstrap) = &bootstrap {
        println!(
            "First-credential web bootstrap is armed: open http://{addr}{}",
            bootstrap.url_path()
        );
        if bootstrap.allow_remote {
            println!(
                "Remote bootstrap is enabled; ensure this address is protected by TLS, a proxy, or a trusted network."
            );
        } else {
            println!("Bootstrap requests from non-loopback peers will be rejected.");
        }
    }

    // `with_connect_info` gives the peer's `SocketAddr` to any handler/layer
    // that extracts `ConnectInfo<SocketAddr>` — the oauth rate limiter (M14,
    // ADR 0054) is the one consumer today.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("server error")?;

    // HTTP has drained; now wind down the sync tasks and wait for them to flush
    // their SDK stores before exiting. This drops every `IndexHandle` the sync
    // engine held, closing the search actor's channel.
    tracing::info!("stopping sync engine");
    let (sync_drained, search_drained) = tokio::join!(
        sync_engine.shutdown(),
        shutdown_search_indexer(index_cancel, index_join),
    );
    if !sync_drained || !search_drained {
        anyhow::bail!(
            "shutdown completed with timed-out tasks: sync_drained={sync_drained}, \
             search_drained={search_drained}"
        );
    }

    Ok(())
}

async fn maybe_offer_web_bootstrap(
    store: &Store,
    config: &Config,
) -> anyhow::Result<Option<axon_api::BootstrapConfig>> {
    if !store
        .first_credential_bootstrap_available()
        .await
        .context("checking first-credential bootstrap state")?
    {
        return Ok(None);
    }

    if config.server.bootstrap_web_auto {
        // Headless / container path (ADR 0052): there is no TTY to answer the
        // prompt in a detached container, so an explicit opt-in arms the
        // bootstrap directly. The access code is still unguessable and the
        // loopback/allow_remote gate still applies.
        tracing::info!(
            "first-credential web bootstrap armed non-interactively (server.bootstrap_web_auto)"
        );
    } else {
        // Interactive path: offer it only on a TTY, and only if the operator agrees.
        if !std::io::stdin().is_terminal() {
            return Ok(None);
        }
        let allowed = init::prompt_yes_no(
            "No Matrix accounts or client credentials were found. Allow one first credential to be created through the web interface?",
            true,
        )?;
        if !allowed {
            tracing::info!("first-credential web bootstrap declined by operator");
            return Ok(None);
        }
    }

    tracing::info!(
        allow_remote = config.server.bootstrap_web_allow_remote,
        web_client_url_configured = config.server.web_client_url.is_some(),
        "first-credential web bootstrap armed"
    );
    Ok(Some(axon_api::BootstrapConfig::new(
        config.server.bootstrap_web_allow_remote,
        generate_bootstrap_access_code(),
        config.server.web_client_url.clone(),
    )))
}

fn generate_bootstrap_access_code() -> String {
    let mut rng = rand::rng();
    (0..6)
        .map(|_| {
            BOOTSTRAP_CODE_ALPHABET[rng.random_range(0..BOOTSTRAP_CODE_ALPHABET.len())] as char
        })
        .collect()
}

async fn shutdown_search_indexer(
    index_cancel: Option<tokio_util::sync::CancellationToken>,
    index_join: Option<tokio::task::JoinHandle<()>>,
) -> bool {
    // The search actor normally exits once all IndexHandles are dropped (channel
    // close). However, the matrix-sdk keeps internal background tasks that hold an
    // Arc to the Client — and the PersistContext (with its IndexHandle) lives inside
    // that Arc — so the channel may never close. Cancel the actor directly to ensure
    // a prompt exit; the durable outbox self-heals any un-drained work on next boot.
    if let Some(cancel) = index_cancel {
        cancel.cancel();
    }
    let Some(join) = index_join else {
        return true;
    };

    tracing::info!("stopping search indexer");
    match tokio::time::timeout(SEARCH_SHUTDOWN_DRAIN_TIMEOUT, join).await {
        Ok(_) => true,
        Err(_) => {
            tracing::warn!(
                timeout_secs = SEARCH_SHUTDOWN_DRAIN_TIMEOUT.as_secs(),
                "search indexer did not stop within the timeout; continuing shutdown"
            );
            false
        }
    }
}

/// Build the search indexer's tuning from config. The channel capacity and idle
/// poll cadence are fixed (not operator-facing); the batch size, seed throttle, and
/// writer heap come from `[search]`.
fn indexer_options(search: &axon_core::SearchConfig) -> axon_search::IndexerOptions {
    axon_search::IndexerOptions {
        // The channel only carries coalescing wakeup hints (the durable obligation
        // is the outbox), so a small bound is plenty; a full channel just drops a
        // redundant hint.
        channel_capacity: 64,
        // Drain at least this often even without a wakeup — the safety net against a
        // dropped hint, and the steady-state poll when notifications are quiet.
        idle_poll_interval: std::time::Duration::from_secs(2),
        writer_heap_mb: search.writer_heap_mb,
        batch_size: search.index_batch_size,
        seed_throttle: std::time::Duration::from_millis(search.build_throttle_ms),
    }
}

/// Build the OAuth runtime from `[oauth]` config: construct a
/// `GenericOidcProvider` (discovery-doc fetch) for each of Google/Microsoft
/// that has `enabled = true`, and refuse to boot if Apple is enabled (its
/// provider ships in M14c — silently ignoring the setting would be a worse
/// surprise than a clear boot-time error).
async fn build_oauth_runtime(
    oauth_config: &axon_core::OauthConfig,
) -> anyhow::Result<Arc<axon_api::OAuthRuntime>> {
    anyhow::ensure!(
        !oauth_config.providers.apple.enabled,
        "oauth.providers.apple.enabled = true, but Sign in with Apple support ships in M14c \
         (ADR 0054) — disable it or wait for that milestone"
    );
    anyhow::ensure!(
        oauth_config
            .external_base_url
            .as_deref()
            .is_some_and(|url| !url.is_empty()),
        "oauth.external_base_url is required when oauth.enabled = true (it becomes the base of \
         every upstream provider's redirect_uri)"
    );

    let http = axon_api::oauth_http_client();
    let mut providers: std::collections::HashMap<&'static str, Arc<dyn axon_api::OidcProvider>> =
        std::collections::HashMap::new();

    if let Some(provider) =
        discover_generic_provider("google", &oauth_config.providers.google, &http).await?
    {
        providers.insert("google", provider);
    }
    if let Some(provider) =
        discover_generic_provider("microsoft", &oauth_config.providers.microsoft, &http).await?
    {
        providers.insert("microsoft", provider);
    }

    Ok(Arc::new(axon_api::OAuthRuntime::new(
        oauth_config,
        providers,
    )))
}

/// Confirm `provider_name`'s config carries everything `GenericOidcProvider`
/// needs (assumes the caller already checked `config.enabled`) — shared
/// between server boot ([`discover_generic_provider`]) and `axon oauth
/// bind`'s own pre-flight check ([`crate::oauth::run`]), so the two can't
/// silently disagree about what "configured" means: without this, the CLI
/// could print a bind URL for a provider the running server never actually
/// registered because its `issuer`/`client_id`/`client_secret` were missing.
pub(crate) fn require_generic_provider_configured<'a>(
    provider_name: &str,
    config: &'a axon_core::GenericOauthProviderConfig,
) -> anyhow::Result<(&'a str, &'a str, &'a str)> {
    let issuer = config.issuer.as_deref().with_context(|| {
        format!("oauth.providers.{provider_name}.issuer is required when enabled")
    })?;
    let client_id = config.client_id.as_deref().with_context(|| {
        format!("oauth.providers.{provider_name}.client_id is required when enabled")
    })?;
    let client_secret = config.client_secret.as_deref().with_context(|| {
        format!("oauth.providers.{provider_name}.client_secret is required when enabled")
    })?;
    Ok((issuer, client_id, client_secret))
}

/// Fetch `provider_name`'s discovery document and build its
/// `GenericOidcProvider`, or `Ok(None)` if it isn't enabled. A missing
/// `issuer`/`client_id`/`client_secret` on an enabled provider is a clear
/// boot-time error rather than a confusing runtime 404 later.
async fn discover_generic_provider(
    provider_name: &'static str,
    config: &axon_core::GenericOauthProviderConfig,
    http: &reqwest::Client,
) -> anyhow::Result<Option<Arc<dyn axon_api::OidcProvider>>> {
    if !config.enabled {
        return Ok(None);
    }
    let (issuer, client_id, client_secret) =
        require_generic_provider_configured(provider_name, config)?;

    let provider = axon_api::GenericOidcProvider::discover(
        provider_name,
        http.clone(),
        issuer,
        client_id.to_owned(),
        client_secret.to_owned(),
    )
    .await
    .map_err(|err| anyhow::anyhow!("discovering {provider_name} OIDC configuration: {err}"))?;
    Ok(Some(Arc::new(provider) as Arc<dyn axon_api::OidcProvider>))
}

/// Log the process's `RLIMIT_NOFILE` (soft/hard) once at boot. Diagnostic only
/// (see the fd-exhaustion investigation, GH #242): this pins down, from the
/// very first log line, whether a future "Too many open files" crash is
/// hitting a low ceiling inherited from the launching environment (macOS's
/// launchd-wide soft default is 256) versus an actual leak past a generous
/// one.
fn log_fd_limit() {
    #[cfg(unix)]
    {
        let mut rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `rlim` is a valid, correctly-sized out-param for `getrlimit`.
        let ret = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) };
        if ret == 0 {
            tracing::info!(
                soft = %fmt_rlim(rlim.rlim_cur),
                hard = %fmt_rlim(rlim.rlim_max),
                "RLIMIT_NOFILE at boot",
            );
        } else {
            tracing::warn!(
                error = %std::io::Error::last_os_error(),
                "failed to read RLIMIT_NOFILE",
            );
        }
    }
    #[cfg(not(unix))]
    {
        tracing::debug!("RLIMIT_NOFILE is not applicable on this platform");
    }
}

#[cfg(unix)]
fn fmt_rlim(v: libc::rlim_t) -> String {
    if v == libc::RLIM_INFINITY {
        "unlimited".to_string()
    } else {
        v.to_string()
    }
}

/// Periodically log signals relevant to the fd-exhaustion investigation (GH
/// #242): the media cache's currently-open serve handles (the one place this
/// process deliberately holds a file open per in-flight request) alongside its
/// on-disk LRU size. If `open_handles` climbs and never comes back down across
/// several ticks, that isolates the leak to media serving rather than
/// matrix-rust-sdk or Tantivy. Runs for the life of the process — logging has
/// no state to drain, so it isn't wired into the shutdown sequence.
fn spawn_fd_diagnostics(media_cache: axon_media::MediaCache) {
    const INTERVAL: Duration = Duration::from_secs(300);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(INTERVAL);
        tick.tick().await; // first tick fires immediately; skip it, we just booted
        loop {
            tick.tick().await;
            let stats = media_cache.stats();
            tracing::info!(
                open_handles = stats.open_handles,
                cache_entries = stats.entries,
                cache_bytes = stats.total_bytes,
                "media cache fd diagnostics",
            );
        }
    });
}

/// Initialize the `tracing` subscriber. Honors `RUST_LOG` if set, otherwise
/// falls back to the configured log level.
fn init_tracing(level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
}

fn config_env_present() -> bool {
    std::env::var_os("DATABASE_URL").is_some()
        || std::env::vars_os().any(|(key, _)| {
            key.to_str()
                .is_some_and(|key| key.starts_with("AXON_") && key != "AXON_CONFIG")
        })
}

/// Resolve when the process receives Ctrl-C or (on Unix) SIGTERM, so the server
/// drains in-flight requests before exiting — important under a container
/// orchestrator.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
}
