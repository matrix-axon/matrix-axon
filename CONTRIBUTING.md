# Contributing to Axon

Everything you need between `git clone` and a merged pull request.
For what Axon _is_, see [README.md](README.md);
for the working conventions and the current state of each milestone, see [AGENTS.md](AGENTS.md).

## Prerequisites

You do not need all of these.
Take the first block plus whichever silo you plan to touch.

**Always:**

- **Rust, via [rustup](https://rustup.rs).**
  Do not install a specific toolchain by hand — `rust-toolchain.toml` pins the version (1.95.0 today) along with `clippy` and `rustfmt`, and rustup installs it on the first `cargo` command in the checkout.
- **[`pre-commit`](https://pre-commit.com)**, which drives the pre-push gate.
  `pipx install pre-commit`, `uv tool install pre-commit`, `pip install --user pre-commit`, or `sudo apt install pre-commit`.
  `./scripts/setup-hooks.sh` refuses to run without it.

**If you touch `clients/web/` — or `.pre-commit-config.yaml`, which the web hooks also watch:**

- **Node.js 24 (LTS)**, what CI uses.
  The hard floor is **22.13**: pnpm 11 needs the `node:sqlite` builtin and Vite 8 needs 20.19+/22.12+.
  Odd-numbered and older releases fail with an `ERR_UNKNOWN_BUILTIN_MODULE` for `node:sqlite`.
- **pnpm 11**, pinned in `clients/web/package.json`'s `packageManager` field.

**If you run the server, the smoke lanes, or the integration suite:**

- **Docker.**
  Optional for Postgres — a local instance already listening on `127.0.0.1:5432` is used directly — but required for `scripts/smoke-gate.sh` (except the `tui` lane) and for `scripts/integration-test.sh`, which brings up Postgres and Synapse via Compose.

**Optional but recommended:**

- **[jj](https://jj-vcs.github.io/jj/) (Jujutsu)** — the repo is jj-colocated and most work here is done with jj rather than raw git.
  Git works fine;
  if you use jj, you also want `cargo install jj-hooks` so `jj push` runs the same gate git gets (see [AGENTS.md](AGENTS.md) for the alias).
- **Playwright browsers**, for the web end-to-end suite: `pnpm --dir clients/web exec playwright install`.
- **[shellcheck](https://www.shellcheck.net/)** (`sudo apt install shellcheck`, `brew install shellcheck`).
  The `actionlint` hook vendors its own binary, but it shells out to shellcheck for the `run:` blocks inside workflows and **silently skips that analysis when shellcheck is missing** — no warning, just a weaker check.
  GitHub runners ship it, so CI always runs the strict version;
  without it locally, the hook can pass on a workflow CI will reject.

### Platform install commands

#### Ubuntu / WSL2

```bash
sudo apt install docker.io docker-compose-v2 pre-commit
sudo snap install --classic rustup
```

For the web client, install Node with [nvm](https://github.com/nvm-sh/nvm) (Ubuntu's packaged `nodejs` is usually too old):

```bash
nvm install 24 && nvm alias default 24
npm install -g pnpm
```

#### macOS

If you have neither Homebrew, Rust, nor Docker:

```bash
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
brew install rustup node pnpm pre-commit
brew install --cask docker
```

You likely need to start Docker from the desktop the first time and grant it administrative privileges.

> On older Intel Macs, `pnpm` may fail to verify the identity of the `@pnpm/exe.darwin-x64` native binary.
> That is an upstream bug;
> use corepack instead — see [clients/web/README.md](clients/web/README.md).

#### Windows (PowerShell)

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

## First-time setup

### 1. Install the pre-push gate

```bash
./scripts/setup-hooks.sh
```

Once per clone.
See [What runs before a push](#what-runs-before-a-push).

### 2. Start Postgres

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

### 3. Generate a config

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
> Make sure the role and database exist (step 2's "without Docker" block) and that `database.url` in your generated config points at it.
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

For the web client, see [clients/web/README.md](clients/web/README.md) — it has its own dev server, proxy configuration, and test lanes.

## What runs before a push

`.pre-commit-config.yaml` is the **only** list of checks the repo runs before a push, for both VCS front-ends (see [ADR 0092](docs/adr/0092-unified-pre-push-gate.md)).
`./scripts/setup-hooks.sh` installs it for git;
jj users get the identical list through the `jj-hooks` alias in [AGENTS.md](AGENTS.md).

It is path-filtered, so a web-only push does not build rust and a rust-only push does not run pnpm.

**Read `.pre-commit-config.yaml` for what the gate contains.**
It lists the hooks top to bottom in the order they run, each with its command, its path filter, and a comment saying why it is there — so this page does not repeat them.
Two copies of one list is the problem ADR 0092 exists to fix;
a copy here would go stale the first time a hook is added.

**`cargo test --all` runs in no automatic CI job** — `lint-and-test.yml` is `workflow_dispatch`-only and `lint-and-clippy.yml` has no test step — so skipping the hook means the suite has not run at all.
CI does gate `cargo fmt --all --check` and `cargo clippy --all-targets --all-features -- -D warnings` on every push, plus the whole web job on web pull requests.

To skip deliberately:

```bash
SKIP=cargo-test git push     # one hook
git push --no-verify         # all of them
RUN_SMOKE=tui git push       # additionally run a Docker smoke lane
```

Playwright is deliberately not in the gate (browsers and minutes).
`web-e2e.yml` gates web pull requests, and `clients/web/AGENTS.md` § "Definition of done for a UI change" says when to run it locally.

To run the whole gate by hand without pushing:

```bash
pre-commit run --all-files --hook-stage pre-push   # git
jj-hooks run --runner pre-commit --stage pre-push 'main@upstream..@'   # jj
```

> **jj users: point `TMPDIR` at real disk.**
> `jj-hooks` materialises a temporary git worktree under `$TMPDIR` and builds there, so the rust hooks start from a cold `target/` — around 16 GB for this workspace.
> If `/tmp` is a tmpfs (the systemd default on many distributions), the build dies partway through with an `rustc-LLVM ERROR` reporting `Disk quota exceeded` on the output stream — which looks like a compiler bug and is not one:
>
> ```bash
> TMPDIR=/path/on/real/disk jj push
> ```

## Conventions

[AGENTS.md](AGENTS.md) is the full reference — it is written for humans and agents alike.
The rules that most often surprise a first contributor:

- **One silo per PR.**
  A PR touches the TUI, or the web client, or the server — not two.
  A one-line server change a client needs is a separate PR.
- **Non-trivial designs land as a numbered ADR first**, in `docs/adr/`, for review before implementation.
- **A migration that has landed on `main` is immutable**, down to its comments — sqlx checksums the file's whole contents, so any edit is a hard startup failure on every database that already applied it.
  Add a new migration instead.
  `scripts/check-migrations-immutable.sh` enforces this in CI and in the hook.
- **Docs track code in the same PR.**
  Behavior described by `README.md`, `AGENTS.md`, or this file must be updated alongside the change.
- **Break prose lines after sentences, not at a column.**
  One sentence per line, or per long clause.
  Nothing enforces it and nothing reflows — `prettier` runs with `proseWrap: "preserve"`, so it fixes tables, lists and emphasis and never moves a line break in body text.
  The reason is diffs: an unwrapped paragraph makes every edit one enormous changed line, and a hard column wrap makes a one-word edit rewrap the whole paragraph.
  A break per sentence gives a diff that names the sentence that changed.
  This is for prose you write, and for paragraphs you are already rewriting for other reasons.
  `.git-blame-ignore-revs` lists the reformats that have happened, one entry each, and is the only place that does;
  assume a file it does not name is still hard-wrapped at a column.
  To reflow a file, put it in its own PR with nothing else in it, then — once that PR has merged — add its squash SHA to `.git-blame-ignore-revs` in a one-line follow-up.
  That second step is what actually makes `git blame` skip it;
  the isolated PR only makes it possible.
- **Keep a code span on one line** inside a list item or blockquote.
  Whitespace inside backticks is content, so prettier cannot re-indent a continuation line and dedents it out of the block instead — which drops the list indent or the blockquote's `>`.
  It still renders correctly, which is exactly why it goes unnoticed.

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
