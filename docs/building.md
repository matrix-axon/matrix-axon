# Building Axon from source

How to build and run the server and the terminal client from a checkout, and where to find the web client's build instructions.
If you only want to _use_ Axon, [install.md](install.md) is quicker.
If you plan to send a pull request, also read [CONTRIBUTING.md](../CONTRIBUTING.md) for the pre-push gate and conventions.

Short version, with Rust installed and either a local Postgres or Docker:

```bash
git clone https://github.com/matrix-axon/matrix-axon
cd matrix-axon
cargo run -p axon-server -- init   # generates a config + store_key, once
./run.sh                           # axon-server  (.\run.ps1 on Windows)
./run.sh tui                       # axon-tui
```

`run.sh` uses a local Postgres if one is already listening on `127.0.0.1:5432`, and otherwise starts one via Docker Compose, tearing down whatever it started on exit.

## Prerequisites

You do not need all of these.
Take Rust plus whichever client you plan to build.

- **Rust, via [rustup](https://rustup.rs).**
  Do not install a specific toolchain by hand — `rust-toolchain.toml` pins the version (1.96.1 today) along with `clippy` and `rustfmt`, and rustup installs it on the first `cargo` command in the checkout.

**To build the web client (`clients/web/`):**

- **Node.js 24 (LTS)**, what CI uses.
  The hard floor is **22.13**: pnpm 11 needs the `node:sqlite` builtin and Vite 8 needs 20.19+/22.12+.
  Odd-numbered and older releases fail with an `ERR_UNKNOWN_BUILTIN_MODULE` for `node:sqlite`.
- **pnpm 11**, pinned in `clients/web/package.json`'s `packageManager` field.

**For Postgres, if you do not already have one:**

- **Docker.**
  Optional — a local instance already listening on `127.0.0.1:5432` is used directly.
  (Contributors also need it for `scripts/smoke-gate.sh`, except the `tui` lane, and for `scripts/integration-test.sh`, which brings up Postgres and Synapse via Compose.)

## Platform install commands

`pre-commit` below is only needed if you will contribute; see [CONTRIBUTING.md](../CONTRIBUTING.md).

### Ubuntu / WSL2

```bash
sudo apt install docker.io docker-compose-v2 pre-commit
sudo snap install --classic rustup
```

For the web client, install Node with [nvm](https://github.com/nvm-sh/nvm) (Ubuntu's packaged `nodejs` is usually too old):

```bash
nvm install 24 && nvm alias default 24
npm install -g pnpm
```

### macOS

If you have neither Homebrew, Rust, nor Docker:

```bash
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
brew install rustup node pnpm pre-commit
brew install --cask docker
```

You likely need to start Docker from the desktop the first time and grant it administrative privileges.

> On older Intel Macs, `pnpm` may fail to verify the identity of the `@pnpm/exe.darwin-x64` native binary.
> That is an upstream bug;
> use corepack instead — see [clients/web/README.md](../clients/web/README.md).

### Windows (PowerShell)

> WSL2 users should follow the Ubuntu path above instead.

```powershell
winget install Rustlang.Rustup
winget install Docker.DockerDesktop
winget install OpenJS.NodeJS.LTS
```

Start Docker Desktop from the Start menu once and grant it administrative privileges.
PowerShell also restricts local scripts by default;
allow them for your account once:

```powershell
Set-ExecutionPolicy -ExecutionPolicy RemoteSigned -Scope CurrentUser
```

#### Building signed Windows packages

`scripts/package-windows.ps1` builds the server (with the web client), the TUI and the desktop installer, signs them, and writes the same zips and installer a release attaches to `dist-windows/`:

```powershell
.\scripts\package-windows.ps1                 # all three
.\scripts\package-windows.ps1 -SkipDesktop    # -SkipServer, -SkipTui likewise
.\scripts\package-windows.ps1 -NoSign         # unsigned, whatever the environment holds
```

It signs when `ARTIFACT_SIGNING_ENDPOINT`, `ARTIFACT_SIGNING_ACCOUNT` and `ARTIFACT_SIGNING_PROFILE` are all set (the same values CI takes from repository variables; ADR 0119), after you have signed in with `az login` as an identity holding the _Artifact Signing Certificate Profile Signer_ role.
With them unset it builds unsigned packages and says so.
It also needs `pnpm`, and Git for Windows' `bash` (with `grep`) for the embedded-web check, which is skipped with a warning without it.
`scripts/sign-windows.ps1 sign <file>...` signs individual files the same way.

## First-time setup

### 1. Start Postgres

Run from the repo root.

**With Docker (easiest):**

```bash
docker compose up -d postgres
```

**Without Docker** — create the role and database in your local instance:

```bash
psql postgres <<SQL
CREATE ROLE axon LOGIN PASSWORD 'axon';
CREATE DATABASE axon OWNER axon;
SQL
```

The database must be `UTF8`, or the server refuses to start;
see [Database encoding](technical-reference.md#database-encoding) if your cluster was initialized under a `C` locale.

### 2. Generate a config

```bash
cargo run -p axon-server -- init
```

`axon-server init` writes a minimal config — with a generated `sync.store_key` and a Postgres URL — to the platform config directory, or to `--config <PATH>` if you pass one.
Do not use the `change-me` placeholder from the example file for anything real.

The server also loads `.env` automatically on startup.
`.env.example` is a manual reference for development or CI environments that prefer environment variables;
if you copy it, replace `AXON_SYNC__STORE_KEY=change-me` with a real secret and adjust `DATABASE_URL` if your Postgres differs.

> **Local Postgres is detected automatically.**
> If Postgres is already running on `127.0.0.1:5432` (Homebrew, Postgres.app, a system package), `run.sh` / `run.ps1` use it and skip Docker entirely.
> Make sure the role and database exist (step 1's "without Docker" block) and that `database.url` in your generated config points at it.
> For the env-var path, set `DATABASE_URL` in `.env`;
> to change what the launcher probes, set `POSTGRES_HOST` / `POSTGRES_PORT`.
>
> **macOS + Docker:** `localhost` can resolve to IPv6 (`::1`) on macOS, but Docker binds IPv4 only.
> The examples use `127.0.0.1` explicitly.

## Running it

```bash
# Auto-detects local Postgres or starts one via Docker; tears down whatever it
# started on exit, whether by Ctrl-C, SIGTERM, or anything else.
./run.sh          # macOS / Linux / WSL — axon-server (default)
./run.sh tui      # axon-tui
./run.sh clean    # destroys the Postgres data volume and exits (no rebuild)
.\run.ps1         # Windows (PowerShell) — same targets
```

The run scripts are source-checkout scaffolding: they load `.env` if present and run the chosen target.
First-run config and secret generation live in `axon-server init`, not in the shell scripts.
To skip them once Postgres is up:

```bash
cargo run -p axon-server
cargo run -p axon-tui
```

In another shell:

```bash
curl localhost:8080/healthz     # -> {"status":"ok"}
curl -H "Authorization: Bearer <token>" localhost:8080/v1/status
```

If the server starts interactively against a database with no Matrix accounts and no existing client credentials, it offers to arm a one-time web bootstrap.
Accepting prints a per-boot `/bootstrap/<code>` URL where you can create the first bearer token or, when OAuth is configured, bind and mint the first SSO-backed credential.
The code is six unambiguous characters, and the bootstrap locks for the rest of that process after six wrong URLs.
Once any account, token, or OAuth identity exists it closes permanently;
use the CLI/admin paths for later credentials.

For the web client, see [clients/web/README.md](../clients/web/README.md) — it has its own dev server, proxy configuration, and test lanes.

## Start over

To restart with a fresh instance and fresh data, destroy and recreate the Postgres container:

```bash
docker compose down -v postgres
docker compose up -d postgres
```

## Troubleshooting

While Axon is pre-release there may be breaking updates.
An `Error: connecting to database` after `cargo run -p axon-server` usually means a stale volume — start a fresh Postgres container as above.

If startup fails because `sqlx` says an already-applied migration "has been modified", repair the local metadata without dropping the database:

```bash
cargo run -p axon-server --features dev-tools -- db repair-migrations
cargo run -p axon-server --features dev-tools -- db repair-migrations --apply
```

This compares the current embedded migration checksums against `_sqlx_migrations` and rewrites only the metadata rows for matching versions.
It does not touch application tables or Matrix history.
It is for local developer databases after a rebase or an edited historical migration — not for production remediation — which is why it is compiled into `axon-server` only under the `dev-tools` Cargo feature.
