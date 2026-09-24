//! Typed configuration loader.
//!
//! Configuration is layered, lowest precedence first:
//!
//! 1. Struct defaults (`#[serde(default)]`) — everything except `database.url`.
//! 2. A TOML file, if one is found (see [`Config::load_default`]).
//! 3. The bare `DATABASE_URL` environment variable, mapped onto `database.url`
//!    (so the project's existing `.env` / sqlx tooling works unchanged).
//! 4. `AXON_`-prefixed environment variables, nested with `__`
//!    (e.g. `AXON_SERVER__PORT=9090` sets `server.port`).
//!
//! Later layers override earlier ones.
//!
//! ## On-disk locations
//!
//! The durable SDK store ([`SyncConfig::data_dir`]) and search index
//! ([`SearchConfig::index_path`]) default under the platform **data** directory,
//! the media-upload staging area ([`MediaConfig::uploads_dir`]) under the
//! platform **data** directory, the disposable media cache
//! ([`MediaConfig::cache_dir`]) under the platform **cache** directory, and the
//! config file is discovered from the platform
//! **config** directory (see [`Config::discover_config_path`]). These follow OS
//! conventions — XDG on Linux, `~/Library` on macOS, Known Folders on Windows —
//! via the `directories` crate (ADR 0050). The project directory is `axon-server`
//! (the binary name), matching `axon-tui`. A pre-rename `axon` layout is still
//! *read* so an upgrade does not look at an empty new data dir. Any location can
//! be overridden by its config key or the matching `AXON_*` env var. When no
//! home directory is discoverable (e.g. a stripped-environment container), each
//! falls back to a CWD-relative `axon-data/…` path.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use serde::Deserialize;

use crate::error::ConfigError;

/// Top-level Axon configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// HTTP server bind settings.
    #[serde(default)]
    pub server: ServerConfig,
    /// Postgres connection settings. `database.url` is required.
    pub database: DatabaseConfig,
    /// Logging settings.
    #[serde(default)]
    pub log: LogConfig,
    /// Sync-engine settings: where the matrix-rust-sdk stores live and the
    /// key that encrypts each account's session at rest. Accounts themselves
    /// are added at runtime (`POST /v1/accounts/login` / `/import`), not
    /// provisioned from config.
    #[serde(default)]
    pub sync: SyncConfig,
    /// Full-text search settings: whether the Tantivy index is enabled and
    /// where it lives.
    #[serde(default)]
    pub search: SearchConfig,
    /// Media-proxy cache settings: whether the bounded on-disk LRU cache is
    /// enabled, where it lives, and its size bounds.
    #[serde(default)]
    pub media: MediaConfig,
    /// OAuth 2.0 authorization-server settings: whether axon issues its own
    /// bearer tokens via Apple/Google/Microsoft sign-in (M14, ADR 0054).
    #[serde(default)]
    pub oauth: OauthConfig,
    /// Set when the loaded file was the pre-rename platform path
    /// (`~/.config/axon/axon.toml` and the macOS/Windows equivalents). Not
    /// serialized; the server logs a warning once tracing is up. `None` for a
    /// current-path file, a CWD `./axon.toml`, an explicit `--config` that is
    /// not the legacy path, or env-only loads.
    #[serde(skip)]
    pub legacy_config_path: Option<PathBuf>,
    /// True when at least one omitted dir key was remapped because the
    /// pre-rename path exists on disk and the current `axon-server` path does
    /// not. Independent of [`Self::legacy_config_path`]: an env-only boot has
    /// no config file but can still have leftover `~/.local/share/axon/…`
    /// state. Not serialized; the server logs a warning once tracing is up.
    #[serde(skip)]
    pub used_legacy_data_dirs: bool,
}

/// HTTP server bind settings.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Address to bind. Defaults to `127.0.0.1` (loopback, safe by default;
    /// put a reverse proxy in front for external access).
    #[serde(default = "default_host")]
    pub host: IpAddr,
    /// Port to bind. Defaults to `8080`.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Permit binding a non-loopback address while Axon serves plain HTTP.
    ///
    /// Axon terminates no TLS itself, and the `/v1/` API carries credentials
    /// (login passwords, recovery keys, bearer tokens). The tech spec requires
    /// client↔Axon TLS, so the safe deployment is to bind loopback and front
    /// Axon with a TLS-terminating reverse proxy (or a private mesh VPN). The
    /// server therefore **refuses** a non-loopback bind unless this is set —
    /// an explicit, auditable "I accept cleartext on the wire" override for
    /// trusted-network or testing scenarios. Defaults to `false`.
    #[serde(default)]
    pub allow_insecure_bind: bool,
    /// Permit the one-time first-credential web bootstrap from non-loopback
    /// peers. Defaults to `false`, so an armed bootstrap is reachable only from
    /// localhost unless the operator explicitly opts into remote setup.
    ///
    /// The bootstrap surface's six-wrong-URL lockout counter is process-local
    /// and shared by every caller, not per-peer — enabling this turns that
    /// counter into a remotely triggerable denial of service against the
    /// operator's own setup flow (six bad requests from anywhere permanently
    /// close bootstrap for the rest of the process). Only set this behind
    /// TLS, a proxy, or a trusted network, same as any other non-loopback
    /// exposure.
    #[serde(default)]
    pub bootstrap_web_allow_remote: bool,
    /// Optional URL for the human-facing web client. When set, the one-time
    /// bootstrap success pages link there after showing the freshly minted
    /// credential. This is deliberately separate from `oauth.external_base_url`,
    /// which names the Axon API's public callback base.
    #[serde(default)]
    pub web_client_url: Option<String>,
    /// Arm the one-time first-credential web bootstrap **non-interactively**,
    /// for headless / container deployments. Normally the bootstrap is offered
    /// only on an interactive TTY (the operator answers a yes/no prompt); in a
    /// detached container there is no TTY, so it would never arm. When this is
    /// true and no credential yet exists, the server arms the bootstrap at
    /// startup without prompting and logs its URL. Defaults to `false`,
    /// preserving the interactive behavior. The loopback / `allow_remote`
    /// gate still applies — behind a reverse proxy every peer looks remote, so
    /// a proxied deployment also needs `bootstrap_web_allow_remote = true`.
    #[serde(default)]
    pub bootstrap_web_auto: bool,
}

/// Postgres connection settings.
#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    /// Postgres connection URL, e.g. `postgres://axon:axon@localhost:5432/axon`.
    /// Required — supplied via `DATABASE_URL`, `AXON_DATABASE__URL`, or the
    /// `[database].url` key in the TOML file.
    pub url: String,
    /// Maximum size of the connection pool. Defaults to `5`.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

/// Logging settings.
#[derive(Debug, Clone, Deserialize)]
pub struct LogConfig {
    /// `tracing` filter directive, e.g. `info` or `axon=debug,sqlx=warn`.
    /// Overridden by the `RUST_LOG` environment variable when set.
    #[serde(default = "default_log_level")]
    pub level: String,
}

/// Sync-engine settings.
#[derive(Debug, Clone, Deserialize)]
pub struct SyncConfig {
    /// Directory under which the matrix-rust-sdk state + crypto stores live,
    /// one subdirectory per account. Must be durable — losing it forces a
    /// re-login and the loss of historical Megolm sessions.
    #[serde(default = "default_sync_data_dir")]
    pub data_dir: PathBuf,
    /// Symmetric key used to encrypt the access token at rest (pgcrypto
    /// `pgp_sym_encrypt`) and to passphrase the SDK's SQLite store. Required
    /// once any account exists (via runtime login/import); the sync layer
    /// surfaces a readable error if it's missing when needed.
    #[serde(default)]
    pub store_key: Option<String>,
    /// Matrix OAuth client-registration policy used by QR login (ADR 0097).
    /// This is distinct from top-level [`OauthConfig`], where Axon acts as an
    /// authorization server for its own `/v1/` clients.
    #[serde(default)]
    pub matrix_oauth: MatrixOAuthConfig,
    /// Per-room timeline window the sliding-sync list requests (`n`). The SDK
    /// default is `1` (latest event only); we raise it so each room archives its
    /// last N events, giving the timeline read real depth to paginate. Since M10
    /// this is only a cold-start latency knob — the shallow window a room shows
    /// the instant it syncs — not the history bound: the backfill engine extends
    /// each room back toward its start in the background (ADR 0015, ADR 0043).
    /// Defaults to 20.
    #[serde(default = "default_timeline_limit")]
    pub timeline_limit: u32,
    /// Capacity of the live-event broadcast bus that feeds the `/v1/ws`
    /// WebSocket: the number of recent events the bus retains for a connected
    /// client before, if that client can't keep up, the oldest are dropped and
    /// it is told it lagged. Larger values tolerate slower/burstier clients at
    /// the cost of a bigger in-memory ring buffer; it never back-pressures sync.
    /// Defaults to 1024.
    #[serde(default = "default_live_event_buffer")]
    pub live_event_buffer: usize,
    /// Allowlist of *room-scoped* ephemeral event types (`m.typing`,
    /// `m.receipt`, …) forwarded verbatim onto the live-event bus as
    /// `LiveFrame::Ephemeral` (ADR 0056). An event type not on this list is
    /// dropped (logged at debug level) — the allowlist fails closed, so a
    /// future EDU type Axon hasn't reasoned about isn't forwarded by
    /// accident. An empty list disables ephemeral forwarding entirely.
    /// Defaults to `["m.typing", "m.receipt"]`.
    ///
    /// `m.presence` is deliberately absent from the default, and adding it
    /// here is **not sufficient** to enable it: presence is account-scoped,
    /// not room-scoped, and dispatches through a structurally different
    /// sync-engine handler this allowlist doesn't reach — forwarding it needs
    /// a second handler registration (real code), not just this config
    /// change, on top of settling the lag-domain question (see the ADR).
    /// `axon-sync` logs a warning at boot if `m.presence` is listed here
    /// anyway, since it would otherwise silently never be forwarded.
    #[serde(default = "default_ephemeral_event_types")]
    pub ephemeral_event_types: Vec<String>,
    /// Per-call timeout for an outbound message-like mutation — `send`, `edit`,
    /// `redact`, `react` (ADR 0030, issue #241) — in seconds. Defense-in-depth
    /// against a slow `SyncService`: even with the client-side `sync_state`
    /// guard (ADR 0030 decision #3), a caller that doesn't implement it (a
    /// bare `curl`, a future third-party client) could otherwise block inside
    /// `room.send()` for as long as the SDK's own megolm-session-import wait
    /// takes on a fresh device with a large key-backup import (ADR 0026),
    /// which can exceed a minute. A timeout here surfaces as a `504` instead
    /// of an indefinitely hung connection. Kept shorter than the TUI's own
    /// 60 s client-side timeout so the server always answers first. Defaults
    /// to 30.
    #[serde(default = "default_send_mutation_timeout_secs")]
    pub send_mutation_timeout_secs: u64,
    /// Per-call timeout for an outbound ephemeral send — a read receipt (ADR
    /// 0067) or a typing notice (ADR 0068, M19a) — in seconds. Bounds a hung
    /// homeserver response so the request-handling task can't block on it
    /// unbounded. Defaults to 10.
    #[serde(default = "default_ephemeral_send_timeout_secs")]
    pub ephemeral_send_timeout_secs: u64,
    /// Per-call timeout for an outbound room-membership mutation — `leave`,
    /// `forget`, `invite`, `kick`, `ban`, `unban` (ADR 0068, M19b) — or an
    /// outbound room-settings mutation — `set_name`, `set_topic`,
    /// `remove_avatar`, `set_tag`, `remove_tag` (ADR 0068, M19d) — in
    /// seconds. Both groups share this one timeout: each is a purely
    /// local-room state or account-data write with no federation resolution
    /// involved, so there's no principled reason to tune them separately.
    /// Bounds a hung homeserver response so the request-handling task can't
    /// block on it unbounded (AGENTS.md's "every outbound call gets one"
    /// boundary-robustness rule). Defaults to 15. (Avatar-set is the one
    /// M19d verb that does a real media upload, so it reuses
    /// `media.upstream_upload_timeout_secs` instead of this value — see
    /// `axon-server`'s `GatewayAdapter`.)
    #[serde(default = "default_membership_mutation_timeout_secs")]
    pub membership_mutation_timeout_secs: u64,
    /// Per-call timeout for an outbound room-entry mutation — `join`,
    /// `knock`, `create_room`, `create_dm` (ADR 0068, M19c) — in seconds.
    /// Longer than `membership_mutation_timeout_secs` because these can
    /// involve federation resolution (an alias/`server_names` join) rather
    /// than a purely local-room state write. Defaults to 30.
    #[serde(default = "default_room_entry_timeout_secs")]
    pub room_entry_timeout_secs: u64,
    /// Enable the M10 history-backfill engine: a continuous, throttled background
    /// task that pages each joined room's pre-existing history backward through
    /// the same ingestion path as live sync (ADR 0043). Defaults to `true`.
    #[serde(default = "default_backfill_enabled")]
    pub backfill_enabled: bool,
    /// When `true`, the boot-time UTD re-decryption sweep retries every pending
    /// UTD on every process start. The default (`false`) attempts each row once
    /// at startup, then relies on the room-key arrival stream or an explicit
    /// manual retry to back-fill it later.
    #[serde(default)]
    pub always_redecrypt_utds_on_startup: bool,
    /// Events requested per `/messages` page during backfill. Larger pages
    /// backfill faster but hold the connection longer per request. Defaults to 100.
    #[serde(default = "default_backfill_page_size")]
    pub backfill_page_size: u32,
    /// Per-room bound on how many events backfill pulls before stopping (without
    /// marking the room complete, so raising the cap later resumes it). `0` means
    /// "to room start" — unbounded — which is safe because backfill is continuous
    /// and throttled. Defaults to 0.
    #[serde(default)]
    pub backfill_target_depth: u64,
    /// Delay between backfill pages, in milliseconds — the throttle that keeps
    /// backfill from starving live sync. Defaults to 1000.
    #[serde(default = "default_backfill_throttle_ms")]
    pub backfill_throttle_ms: u64,
    /// Per-request timeout for a backfill `/messages` page, in seconds. Bounds a
    /// hung homeserver request so it can't stall account teardown / server
    /// shutdown (the drain awaits the backfill task). Defaults to 30.
    #[serde(default = "default_backfill_page_timeout_secs")]
    pub backfill_page_timeout_secs: u64,
    /// When every joined room is complete or capped, how long backfill idles
    /// before re-polling the room list, in seconds. This is how a newly joined or
    /// re-joined room is picked up without a restart. Defaults to 45.
    #[serde(default = "default_backfill_idle_poll_secs")]
    pub backfill_idle_poll_secs: u64,
    /// Filesystem the backfill disk-space valve watches (via `statvfs`). Defaults
    /// to the sync `data_dir`. **Backfill's primary growth is the Postgres
    /// `events` table**, whose free space axon cannot measure when Postgres is on
    /// another host or a separate volume — point this at the filesystem holding
    /// the Postgres data directory (or the search index) when it differs from
    /// `data_dir`, or monitor that disk separately. On the common single-host /
    /// single-volume deploy the default already reflects Postgres's free space.
    #[serde(default)]
    pub backfill_disk_guard_path: Option<PathBuf>,
    /// Backfill pauses while free space on the guarded filesystem
    /// (`backfill_disk_guard_path`) is below this many bytes — a safety valve so
    /// unbounded backfill can't fill the disk. Live sync is unaffected. Defaults
    /// to 2 GiB.
    #[serde(default = "default_backfill_min_free_bytes")]
    pub backfill_min_free_bytes: u64,
    /// Backfill also pauses when free space is below this percentage of the
    /// filesystem (checked alongside `backfill_min_free_bytes`; either triggers a
    /// pause). Defaults to 5.0.
    #[serde(default = "default_backfill_min_free_percent")]
    pub backfill_min_free_percent: f64,
    /// When `true`, leaving (or being banned from) a room destructively purges its
    /// stored events, state, and search documents (ADR 0044). When `false` (the
    /// default), left rooms are retained but hidden from search by the membership
    /// filter; re-joining restores them.
    #[serde(default)]
    pub purge_on_leave: bool,
}

/// Matrix authorization-server registration settings (ADR 0097).
#[derive(Debug, Clone, Deserialize)]
pub struct MatrixOAuthConfig {
    /// Total budget for each OAuth discovery or dynamic-registration request.
    /// Defaults to 15 seconds.
    #[serde(default = "default_matrix_oauth_request_timeout_secs")]
    pub request_timeout_secs: u64,
    /// Public client IDs provisioned out-of-band for servers that do not allow
    /// dynamic client registration. Map keys are operator-chosen labels; each
    /// value names a homeserver or issuer URL.
    #[serde(default)]
    pub static_registrations: BTreeMap<String, MatrixOAuthStaticRegistration>,
}

/// One operator-provided public Matrix OAuth client registration.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct MatrixOAuthStaticRegistration {
    /// Canonical homeserver or authorization-server issuer URL.
    pub server_url: String,
    /// Public OAuth client identifier. Client secrets are unsupported.
    pub client_id: String,
}

fn default_matrix_oauth_request_timeout_secs() -> u64 {
    15
}

impl Default for MatrixOAuthConfig {
    fn default() -> Self {
        Self {
            request_timeout_secs: default_matrix_oauth_request_timeout_secs(),
            static_registrations: BTreeMap::new(),
        }
    }
}

/// Full-text search (Tantivy) settings.
#[derive(Debug, Clone, Deserialize)]
pub struct SearchConfig {
    /// Whether the search index is built and served. When `false`, the search
    /// indexer and the `GET /v1/search` query surface are disabled (the endpoint
    /// returns `503`); store writes still append `search_outbox` obligations, so
    /// re-enabling search catches up the work it missed. Defaults to `true`.
    #[serde(default = "default_search_enabled")]
    pub enabled: bool,
    /// Directory holding the Tantivy index. Must be durable; lives on the same
    /// disk as Postgres and inherits the operator's filesystem-level encryption
    /// (the index holds decrypted message text — see the tech spec). Defaults to
    /// the platform data directory's `axon-server/search`.
    #[serde(default = "default_search_index_path")]
    pub index_path: PathBuf,
    /// Rows streamed per batch by the indexer — both the corpus seed and the
    /// outbox drain. Bounds the indexer's working set and its share of the
    /// Postgres connection pool. Defaults to 1000.
    #[serde(default = "default_search_index_batch_size")]
    pub index_batch_size: i64,
    /// Delay between corpus-seed batches, in milliseconds. A throttle so a large
    /// from-scratch seed never starves live sync or API reads. Defaults to 0 (no
    /// delay).
    #[serde(default)]
    pub build_throttle_ms: u64,
    /// Heap budget for the single Tantivy `IndexWriter`, in megabytes. Bounds the
    /// indexer's memory. Defaults to 50.
    #[serde(default = "default_search_writer_heap_mb")]
    pub writer_heap_mb: usize,
    /// Maximum number of `GET /v1/search` queries executed concurrently. Each
    /// query runs on a blocking thread and keeps a Tantivy reader busy, so this
    /// caps how much of the blocking pool authenticated searches can occupy;
    /// requests over the limit queue rather than being rejected. Defaults to 8.
    #[serde(default = "default_search_max_concurrent_queries")]
    pub max_concurrent_queries: usize,
    /// Per-query wall-clock budget for `GET /v1/search`, in milliseconds. A query
    /// exceeding it returns `503` (the offset cap already bounds per-query work;
    /// this is a latency backstop). Defaults to 10000 (10s).
    #[serde(default = "default_search_query_timeout_ms")]
    pub query_timeout_ms: u64,
}

/// Media-proxy cache settings.
///
/// The media proxy (`GET /v1/media/…`) downloads and decrypts MXC content
/// through the account's homeserver connection and caches the plaintext bytes
/// in a bounded LRU cache on local disk. The homeserver is the source of truth;
/// this cache is a bounded convenience, never durable storage (there is
/// deliberately no S3 backend — see the implementation spec).
#[derive(Debug, Clone, Deserialize)]
pub struct MediaConfig {
    /// Whether the on-disk cache is used. When `false`, media is still proxied
    /// and served (each request fetches from the homeserver into a short-lived
    /// temporary file, serves it — with range support — then deletes it), but
    /// nothing is retained across requests. Defaults to `true`.
    #[serde(default = "default_media_enabled")]
    pub enabled: bool,
    /// Directory holding the media cache, one subdirectory per account
    /// (`<cache_dir>/<account_id>/`). Need not be durable — a lost cache simply
    /// re-fetches from the homeserver. Defaults to the platform cache
    /// directory's `axon-server/media`.
    #[serde(default = "default_media_cache_dir")]
    pub cache_dir: PathBuf,
    /// Durable staging directory for client-originated media uploads. Pending
    /// uploads are in-flight local mutations, so they live under the platform
    /// data directory rather than the disposable cache. Defaults to the platform
    /// data directory's `axon-server/uploads`.
    #[serde(default = "default_media_uploads_dir")]
    pub uploads_dir: PathBuf,
    /// Total cache size cap, in bytes. When a fetch would push the cache over
    /// this, least-recently-used entries are evicted until it fits. Defaults to
    /// 5 GiB.
    #[serde(default = "default_media_max_bytes")]
    pub max_bytes: u64,
    /// Per-object size cap, in bytes. A single media object larger than this is
    /// never cached (and the request is refused rather than buffered), so one
    /// object cannot blow the total cap or the process's memory. Defaults to
    /// 100 MiB.
    #[serde(default = "default_media_max_object_bytes")]
    pub max_object_bytes: u64,
    /// Per-request timeout for the upstream homeserver media download, in
    /// seconds. Bounds a hung homeserver so a media request can't await
    /// unbounded. **This bounds the entire download**, not just connection
    /// setup (the SDK media API is a single call), so it must comfortably exceed
    /// the time to transfer a `max_object_bytes` object over the operator's link
    /// — raise it if large media over slow links is expected. Defaults to 60.
    #[serde(default = "default_media_fetch_timeout_secs")]
    pub fetch_timeout_secs: u64,
    /// Maximum number of upstream media downloads in flight at once. Because the
    /// media API is not streaming, each in-flight download buffers its whole
    /// object in memory, so this caps aggregate download memory at roughly
    /// `max_concurrent_downloads × max_object_bytes`; requests over the limit
    /// queue rather than being rejected. Defaults to 16.
    #[serde(default = "default_media_max_concurrent_downloads")]
    pub max_concurrent_downloads: usize,
    /// Maximum accepted client upload body, in bytes. Enforced while streaming
    /// the request to the staging directory. Defaults to 100 MiB.
    #[serde(default = "default_media_max_upload_bytes")]
    pub max_upload_bytes: u64,
    /// Wall-clock budget for accepting one upload request, in seconds. Bounds a
    /// slow or stalled client before the bytes reach any homeserver. Defaults to
    /// 60.
    #[serde(default = "default_media_upload_request_timeout_secs")]
    pub upload_request_timeout_secs: u64,
    /// Wall-clock budget for the later homeserver upload/send step, in seconds.
    /// Declared with M15a so the config shape is stable before the M15b SDK
    /// adapter consumes it. Defaults to 60.
    #[serde(default = "default_media_upstream_upload_timeout_secs")]
    pub upstream_upload_timeout_secs: u64,
    /// Maximum number of client uploads streamed to disk concurrently. Requests
    /// over the limit queue rather than allocating unbounded writers. Defaults
    /// to 4.
    #[serde(default = "default_media_max_concurrent_uploads")]
    pub max_concurrent_uploads: usize,
    /// How long an unsent staged upload remains reusable before boot reconcile
    /// prunes it. Defaults to 24 hours.
    #[serde(default = "default_media_staged_upload_ttl_secs")]
    pub staged_upload_ttl_secs: u64,
}

/// OAuth 2.0 authorization-server settings (M14, ADR 0054).
///
/// Axon is its own OAuth 2.0 Authorization Server to its clients (public
/// clients, PKCE mandatory) and an OIDC Relying Party to Apple/Google/
/// Microsoft. Disabled by default so an existing deployment's behavior is
/// unchanged until an operator opts in.
#[derive(Debug, Clone, Deserialize)]
pub struct OauthConfig {
    /// Whether the `/v1/oauth/*` surface is served at all. When `false`,
    /// every oauth route 404s regardless of provider configuration. Defaults
    /// to `false`.
    #[serde(default)]
    pub enabled: bool,
    /// This axon instance's externally-reachable base URL (e.g.
    /// `https://myaxon.example.com`), used to build the upstream-provider
    /// callback URL and any links a client is redirected to. Required once
    /// `enabled` is `true`; validated at boot (`build_oauth_runtime`) so
    /// the error is human-readable (same pattern as `sync.store_key`).
    #[serde(default)]
    pub external_base_url: Option<String>,
    /// How long a minted access token verifies for, in seconds. A config
    /// default, not a settled policy decision — adjust freely. Defaults to
    /// 3600 (1 hour).
    #[serde(default = "default_oauth_access_token_ttl_secs")]
    pub access_token_ttl_secs: u64,
    /// How long a minted refresh token is redeemable for, in seconds.
    /// Defaults to 2,592,000 (30 days).
    #[serde(default = "default_oauth_refresh_token_ttl_secs")]
    pub refresh_token_ttl_secs: u64,
    /// Statically pre-registered OAuth clients (no dynamic client
    /// registration, RFC 7591). Defaults to empty, which means no client can
    /// complete Path A (an empty `redirect_uri` allow-list matches nothing).
    #[serde(default)]
    pub clients: Vec<OauthClientConfig>,
    /// Per-provider settings.
    #[serde(default)]
    pub providers: OauthProvidersConfig,
}

/// One statically pre-registered OAuth client.
#[derive(Debug, Clone, Deserialize)]
pub struct OauthClientConfig {
    /// The client's id, e.g. `axon-ios`.
    pub client_id: String,
    /// Exact-match allow-list of redirect URIs this client may request in
    /// Path A. Exact match, not prefix — a loose match would let a malicious
    /// app registered under the same `client_id` redirect an authorization
    /// code to itself.
    pub redirect_uris: Vec<String>,
}

/// Per-provider OIDC settings.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OauthProvidersConfig {
    /// Sign in with Apple browser login using deployment-owned credentials.
    #[serde(default)]
    pub apple: AppleOauthConfig,
    /// Google sign-in via `GenericOidcProvider`.
    #[serde(default)]
    pub google: GenericOauthProviderConfig,
    /// Microsoft (Azure AD) sign-in via `GenericOidcProvider`. Its
    /// multi-tenant endpoints (`common`/`organizations`/`consumers`) publish
    /// a `{tenantid}`-templated issuer, handled by `GenericOidcProvider`
    /// itself — not a config concern.
    #[serde(default)]
    pub microsoft: GenericOauthProviderConfig,
}

/// Config shared by Google and Microsoft — both are plain discovery-doc-driven
/// OIDC providers (`GenericOidcProvider`).
#[derive(Clone, Default, Deserialize)]
pub struct GenericOauthProviderConfig {
    /// Whether this provider is wired up. Defaults to `false`.
    #[serde(default)]
    pub enabled: bool,
    /// The provider's OIDC issuer, e.g. `https://accounts.google.com`.
    /// Required when `enabled`; validated at oauth-runtime construction.
    #[serde(default)]
    pub issuer: Option<String>,
    /// The client id axon registered with this provider.
    #[serde(default)]
    pub client_id: Option<String>,
    /// The client secret axon registered with this provider. Stored as a
    /// plain string, not `pgp_sym_encrypt`'d — same reasoning as
    /// `sync.store_key`: nothing to recover, must stay cheap to read.
    #[serde(default)]
    pub client_secret: Option<String>,
}

impl std::fmt::Debug for GenericOauthProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenericOauthProviderConfig")
            .field("enabled", &self.enabled)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Credentialed Sign in with Apple browser settings.
#[derive(Clone, Default, Deserialize)]
pub struct AppleOauthConfig {
    /// Whether Apple browser sign-in is enabled. Defaults to `false`.
    #[serde(default)]
    pub enabled: bool,
    /// The web Services ID Apple issues, used as the OIDC `client_id` for the
    /// web (Path A) flow.
    #[serde(default)]
    pub client_id: Option<String>,
    /// Native app bundle ID(s) whose Sign-in-with-Apple SDK identity tokens
    /// (Path B) carry this as `aud` instead of `client_id`.
    #[serde(default)]
    pub native_audiences: Vec<String>,
    /// Apple Developer team id, used to sign the ES256 client-secret JWT.
    #[serde(default)]
    pub team_id: Option<String>,
    /// The signing key's key id (Apple Developer console).
    #[serde(default)]
    pub key_id: Option<String>,
    /// The PEM-encoded ES256 private key backing `key_id`.
    #[serde(default)]
    pub private_key: Option<String>,
    /// Alternative to inline PEM: a bounded, private regular file.
    #[serde(default)]
    pub private_key_path: Option<std::path::PathBuf>,
    /// This provider's callback URL, e.g.
    /// `https://myaxon.example.com/v1/oauth/apple/callback`.
    #[serde(default)]
    pub redirect_uri: Option<String>,
}

impl std::fmt::Debug for AppleOauthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppleOauthConfig")
            .field("enabled", &self.enabled)
            .field("client_id", &self.client_id)
            .field("native_audiences", &self.native_audiences)
            .field("team_id", &self.team_id)
            .field("key_id", &self.key_id)
            .field(
                "private_key",
                &self.private_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "private_key_path",
                &self.private_key_path.as_ref().map(|_| "[REDACTED]"),
            )
            .field("redirect_uri", &self.redirect_uri)
            .finish()
    }
}

fn default_oauth_access_token_ttl_secs() -> u64 {
    3600
}

fn default_oauth_refresh_token_ttl_secs() -> u64 {
    2_592_000
}

impl Default for OauthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            external_base_url: None,
            access_token_ttl_secs: default_oauth_access_token_ttl_secs(),
            refresh_token_ttl_secs: default_oauth_refresh_token_ttl_secs(),
            clients: Vec::new(),
            providers: OauthProvidersConfig::default(),
        }
    }
}

fn default_host() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

fn default_port() -> u16 {
    8080
}

fn default_max_connections() -> u32 {
    5
}

fn default_log_level() -> String {
    // Bare "info" is unusable for local dev: matrix-rust-sdk's crypto machinery
    // logs routine, expected conditions (a UTD whose key hasn't arrived yet, a
    // gossip request being served, a backup-downloaded room key that doesn't
    // decrypt) at info/warn, and a startup UTD-backfill sweep can emit thousands
    // of such lines in seconds. These two targets mute that noise while still
    // surfacing anything the SDK itself considers a real error.
    // `matrix_sdk::encryption::backups` (not `matrix_sdk_crypto::backups` —
    // verified against the actual log target; an older comment here/in
    // axon.toml.example named the wrong path) is the "couldn't decrypt a room
    // key we downloaded from backups" warning specifically.
    "info,matrix_sdk_crypto=error,matrix_sdk::encryption::backups=error".to_string()
}

/// Current project directory name — the `axon-server` binary, matching `axon-tui`.
const PROJECT_APPLICATION: &str = "axon-server";
/// Pre-rename project directory (`ProjectDirs::from("", "", "axon")`). Read as a
/// fallback so an upgrade does not look at an empty new data dir.
const LEGACY_PROJECT_APPLICATION: &str = "axon";
/// Filename inside the current platform config directory.
const PLATFORM_CONFIG_FILE: &str = "config.toml";
/// Filename inside the pre-rename platform config directory, and the CWD override.
const LEGACY_CONFIG_FILE: &str = "axon.toml";

/// The platform's Axon directory set — data / config / cache roots following OS
/// conventions (XDG on Linux, `~/Library` on macOS, Known Folders on Windows).
///
/// `None` when no home directory is discoverable (e.g. a stripped-environment
/// container); callers fall back to a CWD-relative path in that case.
fn project_dirs() -> Option<directories::ProjectDirs> {
    project_dirs_named(PROJECT_APPLICATION)
}

fn legacy_project_dirs() -> Option<directories::ProjectDirs> {
    project_dirs_named(LEGACY_PROJECT_APPLICATION)
}

fn project_dirs_named(application: &str) -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", application)
}

fn default_sync_data_dir() -> PathBuf {
    data_join(project_dirs(), "sync")
}

fn legacy_sync_data_dir() -> PathBuf {
    data_join(legacy_project_dirs(), "sync")
}

fn data_join(dirs: Option<directories::ProjectDirs>, child: &str) -> PathBuf {
    dirs.map(|d| d.data_dir().join(child))
        .unwrap_or_else(|| cwd_relative_fallback(child))
}

fn cache_join(dirs: Option<directories::ProjectDirs>, child: &str) -> PathBuf {
    dirs.map(|d| d.cache_dir().join(child))
        .unwrap_or_else(|| cwd_relative_fallback(child))
}

fn default_timeline_limit() -> u32 {
    20
}

fn default_live_event_buffer() -> usize {
    1024
}

fn default_ephemeral_event_types() -> Vec<String> {
    vec!["m.typing".to_owned(), "m.receipt".to_owned()]
}

fn default_send_mutation_timeout_secs() -> u64 {
    30
}

fn default_ephemeral_send_timeout_secs() -> u64 {
    10
}

fn default_membership_mutation_timeout_secs() -> u64 {
    15
}

fn default_room_entry_timeout_secs() -> u64 {
    30
}

fn default_backfill_enabled() -> bool {
    true
}

fn default_backfill_page_size() -> u32 {
    100
}

fn default_backfill_throttle_ms() -> u64 {
    1000
}

fn default_backfill_page_timeout_secs() -> u64 {
    30
}

fn default_backfill_idle_poll_secs() -> u64 {
    45
}

fn default_backfill_min_free_bytes() -> u64 {
    2 * 1024 * 1024 * 1024
}

fn default_backfill_min_free_percent() -> f64 {
    5.0
}

fn default_search_enabled() -> bool {
    true
}

fn default_search_index_path() -> PathBuf {
    data_join(project_dirs(), "search")
}

fn legacy_search_index_path() -> PathBuf {
    data_join(legacy_project_dirs(), "search")
}

fn default_search_index_batch_size() -> i64 {
    1000
}

fn default_search_writer_heap_mb() -> usize {
    50
}

fn default_search_max_concurrent_queries() -> usize {
    8
}

fn default_search_query_timeout_ms() -> u64 {
    10_000
}

fn default_media_enabled() -> bool {
    true
}

fn default_media_cache_dir() -> PathBuf {
    cache_join(project_dirs(), "media")
}

fn legacy_media_cache_dir() -> PathBuf {
    cache_join(legacy_project_dirs(), "media")
}

fn default_media_uploads_dir() -> PathBuf {
    data_join(project_dirs(), "uploads")
}

fn legacy_media_uploads_dir() -> PathBuf {
    data_join(legacy_project_dirs(), "uploads")
}

fn cwd_relative_fallback(child: &str) -> PathBuf {
    let path = PathBuf::from("axon-data").join(child);
    tracing::warn!(
        path = %path.display(),
        "could not resolve platform Axon directories; falling back to CWD-relative storage"
    );
    path
}

/// True when `path` is the pre-rename platform config file.
fn path_is_legacy_platform_config(path: &Path) -> bool {
    let Some(legacy) = Config::legacy_platform_config_path() else {
        return false;
    };
    same_path(path, &legacy)
}

fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Remap omitted dir keys onto the pre-rename `axon` layout.
///
/// When `force` is true (the loaded file *is* the legacy platform config),
/// every omitted key uses the old default. Otherwise each omitted key uses
/// the old default only if that path exists on disk and the current
/// `axon-server` path does not — so an env-only upgrade keeps finding its
/// SDK store, and a first install with neither tree still gets the new dirs.
/// Explicit file or env values are left alone.
fn apply_legacy_dir_defaults(figment: &Figment, config: &mut Config, force: bool) -> bool {
    let mut used = false;
    used |= remap_omitted_dir(
        figment,
        "sync.data_dir",
        &mut config.sync.data_dir,
        legacy_sync_data_dir,
        default_sync_data_dir,
        force,
    );
    used |= remap_omitted_dir(
        figment,
        "search.index_path",
        &mut config.search.index_path,
        legacy_search_index_path,
        default_search_index_path,
        force,
    );
    used |= remap_omitted_dir(
        figment,
        "media.cache_dir",
        &mut config.media.cache_dir,
        legacy_media_cache_dir,
        default_media_cache_dir,
        force,
    );
    used |= remap_omitted_dir(
        figment,
        "media.uploads_dir",
        &mut config.media.uploads_dir,
        legacy_media_uploads_dir,
        default_media_uploads_dir,
        force,
    );
    used
}

fn remap_omitted_dir(
    figment: &Figment,
    key: &str,
    slot: &mut PathBuf,
    legacy: fn() -> PathBuf,
    current: fn() -> PathBuf,
    force: bool,
) -> bool {
    if figment.find_value(key).is_ok() {
        return false;
    }
    let legacy_path = legacy();
    let current_path = current();
    // A leftover legacy *config* must not undo a completed data-dir move:
    // if the old tree is gone and the new one exists, keep the new default.
    if force {
        if !legacy_path.exists() && current_path.exists() {
            return false;
        }
        *slot = legacy_path;
        return true;
    }
    if legacy_path.exists() && !current_path.exists() {
        *slot = legacy_path;
        return true;
    }
    false
}

fn default_media_max_bytes() -> u64 {
    5 * 1024 * 1024 * 1024
}

fn default_media_max_object_bytes() -> u64 {
    100 * 1024 * 1024
}

fn default_media_fetch_timeout_secs() -> u64 {
    60
}

fn default_media_max_concurrent_downloads() -> usize {
    16
}

fn default_media_max_upload_bytes() -> u64 {
    100 * 1024 * 1024
}

fn default_media_upload_request_timeout_secs() -> u64 {
    60
}

fn default_media_upstream_upload_timeout_secs() -> u64 {
    60
}

fn default_media_max_concurrent_uploads() -> usize {
    4
}

fn default_media_staged_upload_ttl_secs() -> u64 {
    24 * 60 * 60
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            allow_insecure_bind: false,
            bootstrap_web_allow_remote: false,
            web_client_url: None,
            bootstrap_web_auto: false,
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            data_dir: default_sync_data_dir(),
            store_key: None,
            matrix_oauth: MatrixOAuthConfig::default(),
            timeline_limit: default_timeline_limit(),
            live_event_buffer: default_live_event_buffer(),
            ephemeral_event_types: default_ephemeral_event_types(),
            send_mutation_timeout_secs: default_send_mutation_timeout_secs(),
            ephemeral_send_timeout_secs: default_ephemeral_send_timeout_secs(),
            membership_mutation_timeout_secs: default_membership_mutation_timeout_secs(),
            room_entry_timeout_secs: default_room_entry_timeout_secs(),
            backfill_enabled: default_backfill_enabled(),
            always_redecrypt_utds_on_startup: false,
            backfill_page_size: default_backfill_page_size(),
            backfill_target_depth: 0,
            backfill_throttle_ms: default_backfill_throttle_ms(),
            backfill_page_timeout_secs: default_backfill_page_timeout_secs(),
            backfill_idle_poll_secs: default_backfill_idle_poll_secs(),
            backfill_disk_guard_path: None,
            backfill_min_free_bytes: default_backfill_min_free_bytes(),
            backfill_min_free_percent: default_backfill_min_free_percent(),
            purge_on_leave: false,
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            enabled: default_search_enabled(),
            index_path: default_search_index_path(),
            index_batch_size: default_search_index_batch_size(),
            build_throttle_ms: 0,
            writer_heap_mb: default_search_writer_heap_mb(),
            max_concurrent_queries: default_search_max_concurrent_queries(),
            query_timeout_ms: default_search_query_timeout_ms(),
        }
    }
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            enabled: default_media_enabled(),
            cache_dir: default_media_cache_dir(),
            uploads_dir: default_media_uploads_dir(),
            max_bytes: default_media_max_bytes(),
            max_object_bytes: default_media_max_object_bytes(),
            fetch_timeout_secs: default_media_fetch_timeout_secs(),
            max_concurrent_downloads: default_media_max_concurrent_downloads(),
            max_upload_bytes: default_media_max_upload_bytes(),
            upload_request_timeout_secs: default_media_upload_request_timeout_secs(),
            upstream_upload_timeout_secs: default_media_upstream_upload_timeout_secs(),
            max_concurrent_uploads: default_media_max_concurrent_uploads(),
            staged_upload_ttl_secs: default_media_staged_upload_ttl_secs(),
        }
    }
}

impl Config {
    /// Load configuration from an optional TOML file plus environment overrides.
    ///
    /// Passing `None` skips the file layer and relies on environment variables
    /// and struct defaults — the common case for container deployments.
    pub fn load(path: Option<&Path>) -> Result<Config, ConfigError> {
        let mut figment = Figment::new();

        if let Some(path) = path {
            figment = figment.merge(Toml::file(path));
        }

        figment = figment
            // Map the bare `DATABASE_URL` onto `database.url`.
            .merge(
                Env::raw()
                    .map(|key| {
                        if key == "DATABASE_URL" {
                            "database.url".into()
                        } else {
                            key.into()
                        }
                    })
                    .only(&["database.url"]),
            )
            // `AXON_`-prefixed vars take precedence; `__` denotes nesting.
            .merge(Env::prefixed("AXON_").split("__"));

        let mut config: Config = figment
            .extract()
            .map_err(|err| ConfigError::Figment(Box::new(err)))?;

        if let Some(path) = path.filter(|p| path_is_legacy_platform_config(p)) {
            apply_legacy_dir_defaults(&figment, &mut config, true);
            config.legacy_config_path = Some(path.to_path_buf());
        } else {
            config.used_legacy_data_dirs = apply_legacy_dir_defaults(&figment, &mut config, false);
        }

        Ok(config)
    }

    /// Resolve a config file path, then [`load`](Config::load).
    ///
    /// The file is discovered from (see [`Config::discover_config_path`]): the `AXON_CONFIG`
    /// environment variable, else `./axon.toml`, else
    /// `<platform config dir>/config.toml`, else the pre-rename
    /// `<legacy platform config dir>/axon.toml`, else no file layer.
    pub fn load_default() -> Result<Config, ConfigError> {
        Self::load_from(None)
    }

    /// Load configuration, preferring an explicitly-supplied config file path
    /// (e.g. from the `--config` CLI flag) over environment/convention discovery.
    ///
    /// `Some(path)` uses that file directly; `None` falls back to
    /// [`discover_config_path`](Config::discover_config_path).
    pub fn load_from(explicit: Option<&Path>) -> Result<Config, ConfigError> {
        let path = match explicit {
            Some(path) => {
                if !path.exists() {
                    return Err(ConfigError::MissingConfigFile(path.to_path_buf()));
                }
                Some(path.to_path_buf())
            }
            None => Self::discover_config_path()?,
        };
        Config::load(path.as_deref())
    }

    /// Discover the config file path when none is passed explicitly:
    /// `AXON_CONFIG` if set, else `./axon.toml` if it exists, else
    /// `<platform config dir>/config.toml` if it exists, else the pre-rename
    /// `<legacy platform config dir>/axon.toml` if it exists, else `None`.
    ///
    /// A `None` here is what tells the binary "no configuration is in place" — the
    /// signal `axon init` (ADR 0051) keys its first-run offer on.
    pub fn discover_config_path() -> Result<Option<PathBuf>, ConfigError> {
        if let Ok(explicit) = std::env::var("AXON_CONFIG") {
            let path = PathBuf::from(explicit);
            if !path.exists() {
                return Err(ConfigError::MissingConfigFile(path));
            }
            return Ok(Some(path));
        }
        let cwd = PathBuf::from(LEGACY_CONFIG_FILE);
        if cwd.exists() {
            return Ok(Some(cwd));
        }
        if let Some(path) = Self::platform_config_path().filter(|p| p.exists()) {
            return Ok(Some(path));
        }
        Ok(Self::legacy_platform_config_path().filter(|p| p.exists()))
    }

    /// The platform config-dir target for a generated config
    /// (`<platform config dir>/config.toml`), regardless of whether it exists yet —
    /// the default write location for `axon init` (ADR 0051). `None` when no home
    /// directory is discoverable.
    pub fn platform_config_path() -> Option<PathBuf> {
        project_dirs().map(|d| d.config_dir().join(PLATFORM_CONFIG_FILE))
    }

    /// Pre-rename platform config path (`<legacy platform config dir>/axon.toml`).
    /// Still readable so an upgrade keeps using the existing file; `axon init`
    /// never writes here.
    pub fn legacy_platform_config_path() -> Option<PathBuf> {
        legacy_project_dirs().map(|d| d.config_dir().join(LEGACY_CONFIG_FILE))
    }

    /// True when `path` is the pre-rename platform config file.
    ///
    /// Used by `axon-server init` so `--force` cannot treat that file as a write
    /// target (ADR 0050 amendment: init writes only the current path).
    pub fn is_legacy_platform_config(path: &Path) -> bool {
        path_is_legacy_platform_config(path)
    }

    /// The socket address to bind, derived from `server.host` and `server.port`.
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.server.host, self.server.port)
    }
}

#[cfg(test)]
// figment's `Jail::expect_with` dictates a closure returning `figment::Error`,
// which is large; we cannot box it here.
#[allow(clippy::result_large_err)]
mod tests {
    #[test]
    fn apple_config_debug_redacts_inline_key_and_key_path_even_when_nested() {
        let mut config: super::Config = figment::Figment::from(Toml::string(
            "[database]\nurl = 'postgres://test@localhost/test'",
        ))
        .extract()
        .unwrap();
        config.oauth.providers.apple.private_key = Some("PRIVATE_SENTINEL".into());
        config.oauth.providers.apple.private_key_path = Some("PATH_SENTINEL".into());
        config.oauth.providers.google.client_secret = Some("GOOGLE_SECRET_SENTINEL".into());
        config.oauth.providers.microsoft.client_secret = Some("MICROSOFT_SECRET_SENTINEL".into());
        for output in [
            format!("{:?}", config.oauth.providers.apple),
            format!("{:?}", config.oauth.providers.google),
            format!("{:?}", config.oauth.providers.microsoft),
            format!("{:#?}", config),
        ] {
            assert!(!output.contains("PRIVATE_SENTINEL"));
            assert!(!output.contains("PATH_SENTINEL"));
            assert!(!output.contains("GOOGLE_SECRET_SENTINEL"));
            assert!(!output.contains("MICROSOFT_SECRET_SENTINEL"));
            assert!(output.contains("[REDACTED]"));
        }
    }

    use super::*;

    #[test]
    fn defaults_apply_with_only_database_url() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DATABASE_URL", "postgres://u:p@localhost/db");
            let config = Config::load(None).expect("load");
            assert_eq!(config.server.host, IpAddr::V4(Ipv4Addr::LOCALHOST));
            assert_eq!(config.server.port, 8080);
            assert_eq!(config.database.max_connections, 5);
            assert_eq!(
                config.log.level,
                "info,matrix_sdk_crypto=error,matrix_sdk::encryption::backups=error"
            );
            assert_eq!(config.database.url, "postgres://u:p@localhost/db");
            Ok(())
        });
    }

    #[test]
    fn allow_insecure_bind_defaults_false_and_parses_from_env() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DATABASE_URL", "postgres://u:p@localhost/db");
            assert!(!Config::load(None).expect("load").server.allow_insecure_bind);

            jail.set_env("AXON_SERVER__ALLOW_INSECURE_BIND", "true");
            assert!(Config::load(None).expect("load").server.allow_insecure_bind);
            Ok(())
        });
    }

    #[test]
    fn bootstrap_web_allow_remote_defaults_false_and_parses_from_env() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DATABASE_URL", "postgres://u:p@localhost/db");
            assert!(
                !Config::load(None)
                    .expect("load")
                    .server
                    .bootstrap_web_allow_remote
            );

            jail.set_env("AXON_SERVER__BOOTSTRAP_WEB_ALLOW_REMOTE", "true");
            assert!(
                Config::load(None)
                    .expect("load")
                    .server
                    .bootstrap_web_allow_remote
            );
            Ok(())
        });
    }

    #[test]
    fn bootstrap_web_auto_defaults_false_and_parses_from_env() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DATABASE_URL", "postgres://u:p@localhost/db");
            assert!(!Config::load(None).expect("load").server.bootstrap_web_auto);

            jail.set_env("AXON_SERVER__BOOTSTRAP_WEB_AUTO", "true");
            assert!(Config::load(None).expect("load").server.bootstrap_web_auto);
            Ok(())
        });
    }

    #[test]
    fn web_client_url_defaults_none_and_parses_from_env() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DATABASE_URL", "postgres://u:p@localhost/db");
            assert!(Config::load(None)
                .expect("load")
                .server
                .web_client_url
                .is_none());

            jail.set_env("AXON_SERVER__WEB_CLIENT_URL", "https://axon.example/app");
            assert_eq!(
                Config::load(None)
                    .expect("load")
                    .server
                    .web_client_url
                    .as_deref(),
                Some("https://axon.example/app")
            );
            Ok(())
        });
    }

    #[test]
    fn startup_utd_redecrypt_defaults_false_and_parses_from_env() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DATABASE_URL", "postgres://u:p@localhost/db");
            assert!(
                !Config::load(None)
                    .expect("load")
                    .sync
                    .always_redecrypt_utds_on_startup
            );

            jail.set_env("AXON_SYNC__ALWAYS_REDECRYPT_UTDS_ON_STARTUP", "true");
            assert!(
                Config::load(None)
                    .expect("load")
                    .sync
                    .always_redecrypt_utds_on_startup
            );
            Ok(())
        });
    }

    #[test]
    fn missing_database_url_is_an_error() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            assert!(Config::load(None).is_err());
            Ok(())
        });
    }

    #[test]
    fn env_overrides_file_and_defaults() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "axon.toml",
                r#"
                    [server]
                    port = 7000
                    [database]
                    url = "postgres://file@localhost/db"
                "#,
            )?;
            jail.set_env("AXON_SERVER__PORT", "9090");
            let config = Config::load(Some(Path::new("axon.toml"))).expect("load");
            // env beats file
            assert_eq!(config.server.port, 9090);
            // file value used where env absent
            assert_eq!(config.database.url, "postgres://file@localhost/db");
            Ok(())
        });
    }

    #[test]
    fn socket_addr_combines_host_and_port() {
        let config = Config {
            server: ServerConfig {
                host: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
                port: 1234,
                allow_insecure_bind: false,
                bootstrap_web_allow_remote: false,
                web_client_url: None,
                bootstrap_web_auto: false,
            },
            database: DatabaseConfig {
                url: "x".into(),
                max_connections: 5,
            },
            log: LogConfig::default(),
            sync: SyncConfig::default(),
            search: SearchConfig::default(),
            media: MediaConfig::default(),
            oauth: OauthConfig::default(),
            legacy_config_path: None,
            used_legacy_data_dirs: false,
        };
        assert_eq!(config.socket_addr().to_string(), "0.0.0.0:1234");
    }

    #[test]
    fn search_defaults_when_absent() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DATABASE_URL", "postgres://u:p@localhost/db");
            // Pin the platform data root so the default resolves deterministically
            // regardless of the test runner's real HOME/XDG: Linux honors
            // XDG_DATA_HOME directly, and macOS's `directories` backend falls
            // back to $HOME when it's set (dirs-sys checks $HOME before the
            // OS's real passwd-database home). Windows' SHGetKnownFolderPath
            // can't be redirected via env vars at all, so it gets a
            // suffix-only check below instead of a exact-path one.
            let home = jail.directory().join("home");
            jail.set_env("HOME", home.to_str().expect("utf8"));
            jail.set_env("XDG_DATA_HOME", "/xdg/data");
            let config = Config::load(None).expect("load");
            assert!(config.search.enabled);
            if cfg!(target_os = "macos") {
                assert_eq!(
                    config.search.index_path,
                    home.join("Library/Application Support/axon-server/search")
                );
            } else if cfg!(target_os = "linux") {
                assert_eq!(
                    config.search.index_path,
                    PathBuf::from("/xdg/data/axon-server/search")
                );
            } else {
                assert!(config
                    .search
                    .index_path
                    .ends_with("axon-server/data/search"));
            }
            assert_eq!(config.search.index_batch_size, 1000);
            assert_eq!(config.search.build_throttle_ms, 0);
            assert_eq!(config.search.writer_heap_mb, 50);
            Ok(())
        });
    }

    #[test]
    fn search_can_be_disabled_via_env() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DATABASE_URL", "postgres://u:p@localhost/db");
            jail.set_env("AXON_SEARCH__ENABLED", "false");
            assert!(!Config::load(None).expect("load").search.enabled);
            Ok(())
        });
    }

    #[test]
    fn media_defaults_when_absent() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DATABASE_URL", "postgres://u:p@localhost/db");
            // Disposable cache defaults under the platform cache root, not the
            // data root; pinned the same way as search/sync (see the comment
            // there for why this works cross-platform).
            let home = jail.directory().join("home");
            jail.set_env("HOME", home.to_str().expect("utf8"));
            jail.set_env("XDG_CACHE_HOME", "/xdg/cache");
            let config = Config::load(None).expect("load");
            assert!(config.media.enabled);
            if cfg!(target_os = "macos") {
                assert_eq!(
                    config.media.cache_dir,
                    home.join("Library/Caches/axon-server/media")
                );
                assert_eq!(
                    config.media.uploads_dir,
                    home.join("Library/Application Support/axon-server/uploads")
                );
            } else if cfg!(target_os = "linux") {
                assert_eq!(
                    config.media.cache_dir,
                    PathBuf::from("/xdg/cache/axon-server/media")
                );
                assert!(config.media.uploads_dir.ends_with("axon-server/uploads"));
            } else {
                assert!(config.media.cache_dir.ends_with("axon-server/cache/media"));
                assert!(config
                    .media
                    .uploads_dir
                    .ends_with("axon-server/data/uploads"));
            }
            assert_eq!(config.media.max_bytes, 5 * 1024 * 1024 * 1024);
            assert_eq!(config.media.max_object_bytes, 100 * 1024 * 1024);
            assert_eq!(config.media.fetch_timeout_secs, 60);
            assert_eq!(config.media.max_concurrent_downloads, 16);
            assert_eq!(config.media.max_upload_bytes, 100 * 1024 * 1024);
            assert_eq!(config.media.upload_request_timeout_secs, 60);
            assert_eq!(config.media.upstream_upload_timeout_secs, 60);
            assert_eq!(config.media.max_concurrent_uploads, 4);
            assert_eq!(config.media.staged_upload_ttl_secs, 24 * 60 * 60);
            Ok(())
        });
    }

    #[test]
    fn media_can_be_disabled_and_sized_via_env() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DATABASE_URL", "postgres://u:p@localhost/db");
            jail.set_env("AXON_MEDIA__ENABLED", "false");
            jail.set_env("AXON_MEDIA__MAX_BYTES", "1048576");
            jail.set_env("AXON_MEDIA__MAX_UPLOAD_BYTES", "4096");
            let config = Config::load(None).expect("load");
            assert!(!config.media.enabled);
            assert_eq!(config.media.max_bytes, 1_048_576);
            assert_eq!(config.media.max_upload_bytes, 4096);
            Ok(())
        });
    }

    #[test]
    fn sync_defaults_when_absent() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DATABASE_URL", "postgres://u:p@localhost/db");
            // Durable state defaults under the platform data root; pinned the
            // same way as search/media (see the comment on search above for
            // why this works cross-platform).
            let home = jail.directory().join("home");
            jail.set_env("HOME", home.to_str().expect("utf8"));
            jail.set_env("XDG_DATA_HOME", "/xdg/data");
            let config = Config::load(None).expect("load");
            if cfg!(target_os = "macos") {
                assert_eq!(
                    config.sync.data_dir,
                    home.join("Library/Application Support/axon-server/sync")
                );
            } else if cfg!(target_os = "linux") {
                assert_eq!(
                    config.sync.data_dir,
                    PathBuf::from("/xdg/data/axon-server/sync")
                );
            } else {
                assert!(config.sync.data_dir.ends_with("axon-server/data/sync"));
            }
            assert!(config.sync.store_key.is_none());
            assert_eq!(config.sync.matrix_oauth.request_timeout_secs, 15);
            assert!(config.sync.matrix_oauth.static_registrations.is_empty());
            assert_eq!(config.sync.send_mutation_timeout_secs, 30);
            assert_eq!(config.sync.ephemeral_send_timeout_secs, 10);
            assert_eq!(config.sync.membership_mutation_timeout_secs, 15);
            assert_eq!(config.sync.room_entry_timeout_secs, 30);
            Ok(())
        });
    }

    #[test]
    fn matrix_oauth_registration_config_loads_from_toml() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "axon.toml",
                r#"
                    [database]
                    url = "postgres://u:p@localhost/db"

                    [sync.matrix_oauth]
                    request_timeout_secs = 9

                    [sync.matrix_oauth.static_registrations.mas]
                    server_url = "https://auth.example.org/"
                    client_id = "public-client"
                "#,
            )?;
            let config = Config::load(Some(Path::new("axon.toml"))).expect("load");
            assert_eq!(config.sync.matrix_oauth.request_timeout_secs, 9);
            assert_eq!(
                config.sync.matrix_oauth.static_registrations,
                BTreeMap::from([(
                    "mas".to_owned(),
                    MatrixOAuthStaticRegistration {
                        server_url: "https://auth.example.org/".to_owned(),
                        client_id: "public-client".to_owned(),
                    },
                )])
            );
            Ok(())
        });
    }

    #[test]
    fn matrix_oauth_registration_config_loads_from_env() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DATABASE_URL", "postgres://u:p@localhost/db");
            jail.set_env("AXON_SYNC__MATRIX_OAUTH__REQUEST_TIMEOUT_SECS", "7");
            jail.set_env(
                "AXON_SYNC__MATRIX_OAUTH__STATIC_REGISTRATIONS__MAS__SERVER_URL",
                "https://auth.example.org/",
            );
            jail.set_env(
                "AXON_SYNC__MATRIX_OAUTH__STATIC_REGISTRATIONS__MAS__CLIENT_ID",
                "public-client",
            );
            let config = Config::load(None).expect("load");
            assert_eq!(config.sync.matrix_oauth.request_timeout_secs, 7);
            assert_eq!(
                config.sync.matrix_oauth.static_registrations,
                BTreeMap::from([(
                    "mas".to_owned(),
                    MatrixOAuthStaticRegistration {
                        server_url: "https://auth.example.org/".to_owned(),
                        client_id: "public-client".to_owned(),
                    },
                )])
            );
            Ok(())
        });
    }

    // The XDG mapping (Linux and macOS; see the `_defaults_when_absent` tests
    // above, pinned via `HOME` / `XDG_DATA_HOME` / `XDG_CACHE_HOME` so they're
    // deterministic on both — Windows is documented in ADR 0050 but can't be
    // redirected via env vars, so it gets a suffix-only check there instead).
    // The tests below cover config-file discovery from the platform config dir
    // and CLI-path precedence.

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn config_file_discovered_from_platform_config_dir() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            // Point the XDG config root inside the jail so we can seed a file at
            // <config dir>/axon-server/config.toml and prove `discover_config_path`
            // finds it when neither AXON_CONFIG nor ./axon.toml is present. Pinned
            // via HOME/XDG_CONFIG_HOME the same way as the `_defaults_when_absent`
            // tests (Linux honors XDG_CONFIG_HOME; macOS's `directories` backend
            // honors $HOME). Windows' SHGetKnownFolderPath can't be redirected
            // via env vars at all, so this test is gated off there — running it
            // un-mocked would mean seeding (and asserting on) the real user's
            // AppData directory instead of a throwaway jail path.
            let home = jail.directory().join("home");
            jail.set_env("HOME", home.to_str().expect("utf8"));
            let cfg_home = jail.directory().join("cfg");
            jail.set_env("XDG_CONFIG_HOME", cfg_home.to_str().expect("utf8"));

            let config_dir = if cfg!(target_os = "macos") {
                home.join("Library/Application Support/axon-server")
            } else {
                cfg_home.join("axon-server")
            };
            jail.create_dir(&config_dir)?;
            jail.create_file(
                config_dir.join("config.toml"),
                r#"
                    [database]
                    url = "postgres://cfgdir@localhost/db"
                "#,
            )?;
            let config = Config::load_default().expect("load");
            assert_eq!(config.database.url, "postgres://cfgdir@localhost/db");
            assert!(config.legacy_config_path.is_none());
            Ok(())
        });
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn legacy_config_file_discovered_when_new_path_absent() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let home = jail.directory().join("home");
            jail.set_env("HOME", home.to_str().expect("utf8"));
            let cfg_home = jail.directory().join("cfg");
            jail.set_env("XDG_CONFIG_HOME", cfg_home.to_str().expect("utf8"));
            jail.set_env("XDG_DATA_HOME", "/xdg/data");
            jail.set_env("XDG_CACHE_HOME", "/xdg/cache");

            let config_dir = if cfg!(target_os = "macos") {
                home.join("Library/Application Support/axon")
            } else {
                cfg_home.join("axon")
            };
            jail.create_dir(&config_dir)?;
            jail.create_file(
                config_dir.join("axon.toml"),
                r#"
                    [database]
                    url = "postgres://legacy@localhost/db"
                "#,
            )?;
            let config = Config::load_default().expect("load");
            assert_eq!(config.database.url, "postgres://legacy@localhost/db");
            assert_eq!(
                config.legacy_config_path.as_deref(),
                Some(config_dir.join("axon.toml").as_path())
            );
            if cfg!(target_os = "macos") {
                assert_eq!(
                    config.sync.data_dir,
                    home.join("Library/Application Support/axon/sync")
                );
                assert_eq!(
                    config.search.index_path,
                    home.join("Library/Application Support/axon/search")
                );
                assert_eq!(
                    config.media.cache_dir,
                    home.join("Library/Caches/axon/media")
                );
                assert_eq!(
                    config.media.uploads_dir,
                    home.join("Library/Application Support/axon/uploads")
                );
            } else {
                assert_eq!(config.sync.data_dir, PathBuf::from("/xdg/data/axon/sync"));
                assert_eq!(
                    config.search.index_path,
                    PathBuf::from("/xdg/data/axon/search")
                );
                assert_eq!(
                    config.media.cache_dir,
                    PathBuf::from("/xdg/cache/axon/media")
                );
                assert_eq!(
                    config.media.uploads_dir,
                    PathBuf::from("/xdg/data/axon/uploads")
                );
            }
            Ok(())
        });
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn new_config_path_beats_legacy() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let home = jail.directory().join("home");
            jail.set_env("HOME", home.to_str().expect("utf8"));
            let cfg_home = jail.directory().join("cfg");
            jail.set_env("XDG_CONFIG_HOME", cfg_home.to_str().expect("utf8"));
            jail.set_env("XDG_DATA_HOME", "/xdg/data");

            let (new_dir, old_dir) = if cfg!(target_os = "macos") {
                (
                    home.join("Library/Application Support/axon-server"),
                    home.join("Library/Application Support/axon"),
                )
            } else {
                (cfg_home.join("axon-server"), cfg_home.join("axon"))
            };
            jail.create_dir(&new_dir)?;
            jail.create_dir(&old_dir)?;
            jail.create_file(
                new_dir.join("config.toml"),
                r#"
                    [database]
                    url = "postgres://new@localhost/db"
                "#,
            )?;
            jail.create_file(
                old_dir.join("axon.toml"),
                r#"
                    [database]
                    url = "postgres://legacy@localhost/db"
                "#,
            )?;
            let config = Config::load_default().expect("load");
            assert_eq!(config.database.url, "postgres://new@localhost/db");
            assert!(config.legacy_config_path.is_none());
            if cfg!(target_os = "macos") {
                assert_eq!(
                    config.sync.data_dir,
                    home.join("Library/Application Support/axon-server/sync")
                );
            } else {
                assert_eq!(
                    config.sync.data_dir,
                    PathBuf::from("/xdg/data/axon-server/sync")
                );
            }
            Ok(())
        });
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn env_only_uses_legacy_data_dir_when_it_exists_and_new_does_not() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DATABASE_URL", "postgres://u:p@localhost/db");
            let home = jail.directory().join("home");
            jail.set_env("HOME", home.to_str().expect("utf8"));
            let data_home = jail.directory().join("xdg-data");
            jail.set_env("XDG_DATA_HOME", data_home.to_str().expect("utf8"));
            jail.set_env(
                "XDG_CACHE_HOME",
                jail.directory().join("xdg-cache").to_str().expect("utf8"),
            );

            let (legacy_sync, new_search) = if cfg!(target_os = "macos") {
                (
                    home.join("Library/Application Support/axon/sync"),
                    home.join("Library/Application Support/axon-server/search"),
                )
            } else {
                (
                    data_home.join("axon").join("sync"),
                    data_home.join("axon-server").join("search"),
                )
            };
            std::fs::create_dir_all(&legacy_sync).expect("legacy sync dir");
            let config = Config::load(None).expect("load");
            assert_eq!(config.sync.data_dir, legacy_sync);
            assert!(config.used_legacy_data_dirs);
            assert!(config.legacy_config_path.is_none());
            assert_eq!(config.search.index_path, new_search);
            Ok(())
        });
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn env_only_prefers_new_data_dir_when_both_exist() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DATABASE_URL", "postgres://u:p@localhost/db");
            let home = jail.directory().join("home");
            jail.set_env("HOME", home.to_str().expect("utf8"));
            let data_home = jail.directory().join("xdg-data");
            jail.set_env("XDG_DATA_HOME", data_home.to_str().expect("utf8"));

            let (legacy_sync, new_sync) = if cfg!(target_os = "macos") {
                (
                    home.join("Library/Application Support/axon/sync"),
                    home.join("Library/Application Support/axon-server/sync"),
                )
            } else {
                (
                    data_home.join("axon").join("sync"),
                    data_home.join("axon-server").join("sync"),
                )
            };
            std::fs::create_dir_all(&legacy_sync).expect("legacy sync dir");
            std::fs::create_dir_all(&new_sync).expect("new sync dir");
            let config = Config::load(None).expect("load");
            assert_eq!(config.sync.data_dir, new_sync);
            assert!(!config.used_legacy_data_dirs);
            Ok(())
        });
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn env_only_explicit_data_dir_beats_legacy_on_disk() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DATABASE_URL", "postgres://u:p@localhost/db");
            let home = jail.directory().join("home");
            jail.set_env("HOME", home.to_str().expect("utf8"));
            let data_home = jail.directory().join("xdg-data");
            jail.set_env("XDG_DATA_HOME", data_home.to_str().expect("utf8"));
            jail.set_env("AXON_SYNC__DATA_DIR", "/custom/sync");

            let legacy_sync = if cfg!(target_os = "macos") {
                home.join("Library/Application Support/axon/sync")
            } else {
                data_home.join("axon").join("sync")
            };
            std::fs::create_dir_all(&legacy_sync).expect("legacy sync dir");
            let config = Config::load(None).expect("load");
            assert_eq!(config.sync.data_dir, PathBuf::from("/custom/sync"));
            assert!(!config.used_legacy_data_dirs);
            Ok(())
        });
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn legacy_config_keeps_explicit_data_dir() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let home = jail.directory().join("home");
            jail.set_env("HOME", home.to_str().expect("utf8"));
            let cfg_home = jail.directory().join("cfg");
            jail.set_env("XDG_CONFIG_HOME", cfg_home.to_str().expect("utf8"));
            jail.set_env("XDG_DATA_HOME", "/xdg/data");

            let config_dir = if cfg!(target_os = "macos") {
                home.join("Library/Application Support/axon")
            } else {
                cfg_home.join("axon")
            };
            jail.create_dir(&config_dir)?;
            jail.create_file(
                config_dir.join("axon.toml"),
                r#"
                    [database]
                    url = "postgres://legacy@localhost/db"
                    [sync]
                    data_dir = "/custom/sync"
                "#,
            )?;
            let config = Config::load_default().expect("load");
            assert_eq!(config.sync.data_dir, PathBuf::from("/custom/sync"));
            if cfg!(target_os = "macos") {
                assert_eq!(
                    config.search.index_path,
                    home.join("Library/Application Support/axon/search")
                );
            } else {
                assert_eq!(
                    config.search.index_path,
                    PathBuf::from("/xdg/data/axon/search")
                );
            }
            Ok(())
        });
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn legacy_config_does_not_undo_moved_data_dir() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let home = jail.directory().join("home");
            jail.set_env("HOME", home.to_str().expect("utf8"));
            let cfg_home = jail.directory().join("cfg");
            jail.set_env("XDG_CONFIG_HOME", cfg_home.to_str().expect("utf8"));
            let data_home = jail.directory().join("xdg-data");
            jail.set_env("XDG_DATA_HOME", data_home.to_str().expect("utf8"));

            let (config_dir, new_sync) = if cfg!(target_os = "macos") {
                (
                    home.join("Library/Application Support/axon"),
                    home.join("Library/Application Support/axon-server/sync"),
                )
            } else {
                (
                    cfg_home.join("axon"),
                    data_home.join("axon-server").join("sync"),
                )
            };
            jail.create_dir(&config_dir)?;
            jail.create_file(
                config_dir.join("axon.toml"),
                r#"
                    [database]
                    url = "postgres://legacy@localhost/db"
                "#,
            )?;
            std::fs::create_dir_all(&new_sync).expect("new sync dir");
            let config = Config::load_default().expect("load");
            assert_eq!(config.sync.data_dir, new_sync);
            assert!(config.legacy_config_path.is_some());
            Ok(())
        });
    }

    #[test]
    fn explicit_path_beats_env_and_convention() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            // AXON_CONFIG points at one file; the explicit arg at another.
            jail.create_file(
                "env.toml",
                r#"
                    [database]
                    url = "postgres://env@localhost/db"
                "#,
            )?;
            jail.create_file(
                "explicit.toml",
                r#"
                    [database]
                    url = "postgres://explicit@localhost/db"
                "#,
            )?;
            jail.set_env("AXON_CONFIG", "env.toml");
            let config = Config::load_from(Some(Path::new("explicit.toml"))).expect("load");
            assert_eq!(config.database.url, "postgres://explicit@localhost/db");
            Ok(())
        });
    }

    #[test]
    fn explicit_missing_config_path_is_an_error() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let err = Config::load_from(Some(Path::new("missing.toml"))).expect_err("missing");
            assert!(
                matches!(err, ConfigError::MissingConfigFile(path) if path == Path::new("missing.toml"))
            );
            Ok(())
        });
    }

    #[test]
    fn axon_config_missing_path_is_an_error() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("AXON_CONFIG", "missing-env.toml");
            let err = Config::load_default().expect_err("missing");
            assert!(
                matches!(err, ConfigError::MissingConfigFile(path) if path == Path::new("missing-env.toml"))
            );
            Ok(())
        });
    }
}
