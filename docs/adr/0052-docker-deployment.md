# ADR 0052 — Docker deployment framework

Targets **Milestone 13** (deployment / onboarding; see
`docs/mvp/implementation.md` §13). Builds on the `axon init` first-run bootstrap from
ADR 0051 (PR #208), which is stacked on the platform config-dir discovery in ADR 0050
(PR #206). This ADR can land independently as documentation; the follow-up implementation
PR depends on `axon init` being present on the base branch.

## Context

Axon today has **no application container**. The only container asset is a dev-oriented
`docker-compose.yml` that runs Postgres (and, behind the `integration` profile, a Synapse
test homeserver); the server itself runs on the host via `cargo run` / `run.sh`. ADR 0051
makes the *bare-binary-on-a-host* first run smooth (`axon init` generates a config with a
`store_key` and points the operator at Postgres), but the *everything-in-containers* path
— the one many self-hosters actually want — does not exist.

The M13 goal is "download one thing, run one command, get a working Axon." For a container
audience that means: a single `docker compose up` brings up Postgres **and** `axon-server`
together, fully configured, with an obvious path to put TLS / remote access in front.

Several properties of the server make this tractable (established by reading the tree):

- **Single binary, no runtime shared libs.** The workspace builds one binary, `axon`
  (crate `axon-server`, `[[bin]] name = "axon"`); with no subcommand it runs the server.
  The whole dependency graph is rustls + aws-lc-rs, sqlx's Postgres driver is compiled in
  (no libpq), and there is no OpenSSL / libsqlite3. A `debian:bookworm-slim` runtime needs
  only `ca-certificates` (to reach Matrix homeservers over HTTPS).
- **Postgres is the only hard dependency.** No Redis, no S3/object storage. Media is a
  disposable local LRU cache. Migrations are embedded in the binary and **auto-run at boot**
  inside `Store::connect`; the baseline migration issues `CREATE EXTENSION IF NOT EXISTS
  pgcrypto`, which the `postgres:16` image's superuser role can execute. No separate migrate
  step — the DB just needs to be reachable at start.
- **Config is env-friendly.** `AXON_`-prefixed vars (nested with `__`) override the TOML
  file; bare `DATABASE_URL` is honored. Relevant keys: `AXON_SERVER__HOST` / `__PORT`
  (default `127.0.0.1:8080`), `AXON_SERVER__ALLOW_INSECURE_BIND`, `AXON_SYNC__STORE_KEY`,
  `AXON_SYNC__DATA_DIR`, `AXON_SEARCH__INDEX_PATH`, `AXON_MEDIA__CACHE_DIR`,
  `AXON_MEDIA__UPLOADS_DIR`. Accounts
  are added at runtime via the API, not env/config (ADR 0024).
- **A deliberate bind gate.** The server refuses a non-loopback bind over plain HTTP unless
  `AXON_SERVER__ALLOW_INSECURE_BIND=true` — it serves plain HTTP by design and expects TLS
  to be terminated by a reverse proxy.
- **Orchestrator-friendly runtime.** `GET /healthz` is an unauthenticated liveness probe
  (`{"status":"ok"}`, does not touch the DB); SIGTERM triggers graceful shutdown.
- **A secret that must stay stable.** `store_key` (256-bit) encrypts account tokens at rest
  (pgcrypto) and passphrases the SDK store. Changing it orphans encrypted data. `axon init`
  already generates one via `generate_store_key()` (32 bytes CSPRNG, hex).
- **State classes.** `data_dir` (sync/crypto store) is **durable and critical**; the search
  index is durable-preferred but rebuildable from Postgres; the media cache is disposable.
- **Build needs.** `rust-toolchain.toml` pins `1.95.0`; aws-lc-rs needs cmake + a C compiler
  (and clang/libclang for bindgen) at build time, nothing at runtime. `build.rs` embeds
  `GIT_HASH` from `git rev-parse`, falling back to `"unknown"` when `.git` is absent.

## Decision

Ship a first-party Dockerfile for `axon-server` plus a deployment Compose stack under a new
top-level `deploy/` directory (leaving the existing dev `docker-compose.yml` untouched). Four
decisions shape it:

### 1. Scope: Dockerfile + full-stack Compose

`docker compose up` brings up Postgres and `axon-server` together and yields a fully running
Axon. The image is also usable standalone (operator supplies their own `DATABASE_URL`).
Multi-arch publishing to a registry is done as beta distribution: both images push to a
**private** GHCR package (`ghcr.io/matrix-axon/axon-{server,web}`, linked to the repo via an
`org.opencontainers.image.source` label) via `deploy/publish.sh` or the `publish-images.yml`
workflow (self-hosted runner, `workflow_dispatch` + `beta-*` tag). The compose image refs are
env-overridable (`AXON_SERVER_IMAGE` / `AXON_WEB_IMAGE`) so testers `docker compose pull`
prebuilt images while contributors keep building from source. A public GHCR/Docker Hub
release remains a later step.

### 2. The TUI is not in the server image

`axon-tui` is an interactive per-user client, awkward to run in a server container and
already shipped as native binaries by `cross-build.yml`. The server image stays server-only.

### 3. First-run config via `axon init`, not a bespoke `.env` path

Rather than re-implement CSPRNG key generation and config writing in shell, the container
**reuses `axon init`** (ADR 0051). A thin, idempotent entrypoint runs, on first boot only:

```
[ -f "$AXON_CONFIG" ] || axon init --non-interactive \
    --config "$AXON_CONFIG" --database-url "$DATABASE_URL" --no-token
exec axon --config "$AXON_CONFIG"
```

`$AXON_CONFIG` points at the mounted data volume (`/var/lib/axon/axon.toml`), so the
generated `store_key` + Postgres URL persist; later boots find the file, skip init, and keep
the key stable. The entrypoint **never** passes `--force` (which would regenerate `store_key`
and orphan data). `init` runs migrations via its DB probe; the subsequent server boot re-runs
them idempotently.

The container's only additions on top of `init` are container glue: pointing the config at
the persisted volume, the idempotency guard, passing the Compose DB URL, and defaulting the
bind for container networking — `AXON_SERVER__HOST=0.0.0.0` and
`AXON_SERVER__ALLOW_INSECURE_BIND=true` (these live in env, not the generated config, which
holds only db url + store_key). Both are overridable.

`init`'s `--start-postgres` helper is loopback-guarded and only fires when the DB is
unreachable; in Compose the `postgres` service is up before `axon-server` starts
(`depends_on: service_healthy`) and is not a loopback host, so it never engages — **Compose
owns Postgres, `init` owns config.** The first bearer token is **minted and printed to the
logs on first boot by default** (`AXON_INIT_PRINT_TOKEN` defaults on): the operator is
bootstrapping their own single-admin box and can do nothing without a token, so the
"secrets don't belong in logs" principle yields to bootstrap usability here. It prints once
(first boot only); operators who ship logs off-box set `AXON_INIT_PRINT_TOKEN=false` and mint
out-of-band with `axon token issue`, and any exposed token is revocable with `axon token
revoke`. Matrix accounts are provisioned at runtime only (`POST /v1/accounts/login` or
`POST /v1/accounts/import`), per ADR 0051 and ADR 0024.

### 4. TLS / remote access as optional Compose profiles

The core stack serves plain HTTP for LAN use. Two opt-in profiles front it, covering the two
common self-host situations:

- **`tailscale`** — a `tailscale/tailscale` sidecar on the compose network with a
  `tailscale serve` config proxying HTTPS → `http://axon-server:8080`, giving encrypted
  remote access via the operator's tailnet (or public via Funnel) **with no router
  port-forwarding and nothing exposed to the public internet**. This is the natural fit for
  an Axon box on a home LAN. Tailscale reaches axon over the compose bridge (not a shared
  network namespace) because axon also needs the bridge to reach Postgres — the correct
  pattern for a multi-service app.
- **`caddy`** — a `caddy:2` reverse proxy doing automatic Let's Encrypt HTTPS, for operators
  who have a public domain and will forward port 443 at their router.

In both profiles the server's port 8080 is **not** published to the host; only the proxy is
reachable. axon keeps `0.0.0.0` + `ALLOW_INSECURE_BIND=true`, bridge-internal only.

### 5. The web client rides the stack behind a same-origin front door

The browser web client (`clients/web`, a Preact/Vite SPA) joins the stack as a `web`
service that becomes the **single HTTP front door**. It is a `caddy:2` image built in two
stages — a Node/pnpm stage runs `pnpm build` to produce the static `dist/`, copied into the
Caddy runtime — that serves the SPA and reverse-proxies the API (`/v1/*` and `/healthz`,
including the `/v1/ws` WebSocket) to `axon-server:8080` on the bridge. Three properties of
the client force this shape (established by reading `clients/web`):

- **Same-origin by default.** The client's API base URL defaults to `/` and it has no baked-in
  server URL; the server serves **no CORS headers** and no static assets (it returns a "no web
  interface here" stub at `/`). Serving the SPA and the API from one origin sidesteps CORS
  entirely and needs zero client build config — the alternative (cross-origin + a server CORS
  allow-list) is not implemented and would be more fragile.
- **Browser-compatible auth passes through a proxy.** The client authenticates the WebSocket
  via `Sec-WebSocket-Protocol` (browsers can't set `Authorization`); the TUI uses the
  `Authorization` header. Caddy forwards both and upgrades `/v1/ws` transparently, so a TUI
  (local or remote) can use the **same** front-door URL. `/healthz` gets its own proxied route
  because it lives outside `/v1`.
- **Consequence for the topology.** `axon-server` loses its published host port — `web` owns
  the host-facing `:8080` for the whole stack — and the `tailscale`/`caddy` TLS profiles now
  front `web` (not `axon-server`), so both the browser and the TUI work over TLS same-origin.

Default web sign-in is the **one-time first-credential web bootstrap** (#265), made
zero-paste here. The entrypoint arms it non-interactively (`bootstrap_web_auto`, a new
`ServerConfig` field that skips the TTY prompt for headless runs) and uses `axon init
--no-token` so the bootstrap owns the first credential. Because requests reach axon
*through* the `web` proxy — so axon sees the proxy's bridge IP, never loopback — the
stack also sets `bootstrap_web_allow_remote`; the unguessable one-time code and wrong-URL
lockout remain the protection. The bootstrap's bearer success page, served same-origin
with the SPA via the front door, writes the freshly minted token into the client's
`localStorage` slot and redirects — so the operator lands signed in with nothing to copy,
and the token never travels in a URL. Token paste (any `axon token issue` token) stays
available as a fallback, and OAuth/SSO is an optional upgrade; both are documented in the
deploy README. Minting a token consumes an unused bootstrap (it *is* the first
credential), so `run-docker.sh` defaults to surfacing the bootstrap URL and makes TUI
token minting an explicit `--tui` opt-in.

## Consequences

- One command (`docker compose up`) yields a working, migrated, self-configuring Axon;
  first-run secret generation reuses the tested `axon init` code path rather than shell.
- The durable `data_dir` (and the generated config with its `store_key`) live on a named
  volume; operators must back this up. Losing it forces re-login and loss of historical
  Megolm keys — this must be documented prominently.
- Plain HTTP is the default without a profile; the docs must make the "put a profile (or your
  own proxy) in front before exposing this" caveat unmissable.
- The build image needs cmake + clang and the large dependency graph (matrix-rust-sdk,
  tantivy, aws-lc-rs) compiles slowly; the Dockerfile uses cargo-chef / BuildKit cache mounts
  to keep rebuilds fast.
- Because the implementation depends on `axon init`, the implementation PR must be based on a
  branch that contains ADR 0051's code (the `axon/platform-dirs` stack) or on `main` after
  that stack lands.
- The `web` front door adds a Node/pnpm build stage and puts a reverse proxy in front of all
  API traffic (including the TUI's). The extra in-container hop is negligible; Caddy imposes no
  request-body cap, so media uploads are unaffected. The web-client integration likewise
  depends on `clients/web` being present in the tree, so it lands after the web client reaches
  `main` (or on a branch that combines the two).

## Implementation plan (for the follow-up M13 PR — `deploy/` silo)

Assets (all new; the dev `docker-compose.yml` is untouched):

- **`Dockerfile`** (repo root) — multi-stage. Builder `rust:1.95-bookworm` + `cmake clang`,
  cargo-chef / cache mounts, `cargo build --release -p axon-server --bin axon`, `ARG
  GIT_HASH=unknown`. Runtime `debian:bookworm-slim` + `ca-certificates curl`, non-root user
  (uid 10001), `/var/lib/axon` data dir, `ENV AXON_CONFIG` + `AXON_SYNC__DATA_DIR` /
  `AXON_SEARCH__INDEX_PATH` / `AXON_MEDIA__CACHE_DIR` / `AXON_MEDIA__UPLOADS_DIR`
  under it (`uploads` was previously the XDG default
  `/var/lib/axon/.local/share/axon/uploads` via `HOME=/var/lib/axon`; existing
  rows keep working because they store an absolute path), `VOLUME /var/lib/axon`,
  `EXPOSE 8080`, `HEALTHCHECK curl -fsS localhost:8080/healthz`, entrypoint script.
- **`deploy/entrypoint.sh`** — the thin idempotent `axon init` wrapper described in Decision 3.
- **`deploy/run-docker.sh`** — one-shot operator bootstrap: brings the stack up (`--wait`),
  mints a labelled `tui` token from the running server, and writes a ready-to-use `axon-tui`
  config (`[server].base_url` + `bearer_token`) at the client's platform path, then points the
  operator at the TUI download. Refuses to clobber an existing TUI config (warn + confirm +
  timestamped `.bak` backup on overwrite); `--no-tui` / `--yes` flags.
- **`deploy/docker-compose.yml`** — `postgres` (image `postgres:16`, `pg_isready` healthcheck,
  named volume) + `axon-server` (build/image, `depends_on: service_healthy`, `env_file`,
  `DATABASE_URL=postgres://…@postgres:5432/axon`, `axon-data` volume, `ports 8080:8080` by
  default, `restart: unless-stopped`); `caddy` and `tailscale` services behind Compose
  `profiles`. Named volumes `axon-pgdata`, `axon-data`, `axon-tsstate`.
- **`deploy/.env.example`** — `POSTGRES_*`, `DOMAIN` /
  `ACME_EMAIL` (Caddy), `TS_AUTHKEY` (Tailscale), `AXON_INIT_PRINT_TOKEN`. `store_key` is
  **not** set here (generated by `init` onto the volume); a comment warns that replacing the
  config/volume orphans encrypted data.
- **`deploy/Caddyfile`** — `{$DOMAIN} { reverse_proxy axon-server:8080 }`.
- **`.dockerignore`** (repo root) — exclude `target/`, `axon-data/`, `.git`, `.env`,
  `clients/`, `smoke/`, docs; `GIT_HASH` is passed as a build arg (so `.git` need not be in
  context).
- **`deploy/README.md`** — quick start, the profiles, minting the first token, account
  provisioning, volume/backup notes, and the plain-HTTP security caveat.

Out of scope (later): a GHCR multi-arch publish workflow; Kubernetes/Helm manifests.

### Verification (for the follow-up PR)

1. `cd deploy && cp .env.example .env && GIT_HASH=$(git rev-parse --short HEAD) docker compose
   up --build -d`; `docker compose ps` shows both services healthy; `curl -fsS
   localhost:8080/healthz` → `{"status":"ok"}`.
2. Migrations ran — logs show "running database migrations"; `psql -c '\dt'` lists Axon tables
   + `_sqlx_migrations`.
3. `axon init` ran once and `store_key` is stable — logs show "Wrote configuration to
   /var/lib/axon/axon.toml" on first boot only; `docker compose down && up -d` skips init (no
   such line) and the `store_key` in `$AXON_CONFIG` is unchanged.
4. First token — `docker compose exec axon-server axon token issue --label default` prints a
   token; `curl -H "Authorization: Bearer <tok>" localhost:8080/v1/status` returns 200.
5. Account provisioning — `POST /v1/accounts/login` (or `/import`), confirm sync starts.
   Use only a test homeserver account.
6. `caddy` profile — `docker compose --profile caddy up -d` with `DOMAIN` set; HTTPS reaches
   `/healthz` through Caddy.
7. `tailscale` profile — with a valid `TS_AUTHKEY`, `docker compose --profile tailscale up -d`;
   `tailscale serve status` shows the mapping; reach `https://<device>.<tailnet>.ts.net/healthz`
   from another tailnet device.
8. Graceful shutdown — `docker compose stop axon-server`; logs show a clean drain, no error exit.
