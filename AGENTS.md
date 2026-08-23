# Axon — Contributor Orientation

Axon is a self-hosted personal agent for Matrix: a persistent state layer (sync, E2EE decryption, search, media proxy) that sits between a user's homeserver(s) and their clients.
Arbitrary clients consume it through a stable, versioned HTTP + WebSocket API at `/v1/`.
See `docs/mvp/prd.md` for the full product description.

## Docs

| File                          | Contents                                                                                                                                                                                                                                                  |
| ----------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `docs/mvp/prd.md`             | Product requirements — what we're building and why                                                                                                                                                                                                        |
| `docs/mvp/tech-spec.md`       | Architecture decisions and tradeoffs                                                                                                                                                                                                                      |
| `docs/mvp/implementation.md`  | Milestone-by-milestone build plan (authoritative for agentic contributors)                                                                                                                                                                                |
| `docs/adr/`                   | Architecture decision records — decisions made during implementation                                                                                                                                                                                      |
| `docs/integration-testing.md` | Running axon against a local Synapse (sync + M3c re-decryption) by hand                                                                                                                                                                                   |
| `docs/client-parity.md`       | Human-maintained cross-silo matrix of which `/v1/` capabilities each client (TUI, web, future iOS) actually exposes — update it in the same PR that changes a tracked row's status, from any silo                                                         |
| `docs/demo-coverage.md`       | Human-maintained record of which demo scene shows each visually significant capability (ADR 0086) — update it in the same PR that changes what a client renders, from any silo                                                                            |
| `scripts/demo-stack.sh`       | Brings the ADR 0086 demo world up (`up`/`record`/`down`) for recording client videos by hand. Not a test and not a gate — the stack it starts is meant to stay up while a human records against it.                                                       |
| `scripts/integration-test.sh` | One-command end-to-end re-decryption test: seeds an encrypted room + key backup via `axon-itest`, runs axon as a fresh device, and asserts UTDs back-fill. Runnable in CI on demand via `.github/workflows/integration.yml` (manual `workflow_dispatch`). |

## Directory layout

```text
matrix-axon/
  Cargo.toml                 # workspace
  AGENTS.md                  # this file
  CLAUDE.md                  # one-line pointer to this file
  crates/
    axon-server/             # binary; wires components together
    axon-core/               # shared types, errors, config
    axon-store/              # Postgres + sqlx; event store, account data
    axon-sync/               # matrix-rust-sdk sync engine wrapper
    axon-crypto/             # RESERVED STUB — verification lives in axon-sync (ADR 0027)
    axon-search/             # Tantivy index
    axon-media/              # media proxy + disk-cache backend
    axon-api/                # axum HTTP + WS handlers, OpenAPI (utoipa)
    axon-itest/              # dev-only: integration-test seeder (the `seed` binary)
  clients/
    tui/                     # axon-tui — terminal client for the Axon API, should grow to support all API endpoints as they are enabled
    web/                     # axon-web (Vite + Preact + TS, ADR 0046) — alpha client
  smoke/                     # black-box smoke harnesses (depend on no axon-* crate; ADR 0025)
    tui/                     # axon-smoke-tui — PTY-drives the real axon-tui against an in-process API stub
                             # also ships axon-demo-tui — the ADR 0086 demo pilot (not a test, not a gate)
    server/                  # axon-smoke-server — black-box API/WS smoke against a real stack
    local-stack/             # axon-smoke-local-stack — boots Synapse + Postgres + axon; writes a JSON manifest
      corpus/                # declarative demo content (ADR 0086); rendered by `up --corpus`
        demo.toml            # personas, spaces, rooms, messages — relative timestamps
        media/               # CC0 avatars (committed) + photos (generated placeholders, not committed)
  openapi/                   # OpenAPI 3.1 spec (source of truth)
  docs/
    mvp/                     # PRD, tech spec, implementation spec (frozen at MVP ship)
    adr/                     # architecture decision records
    self-hosting.md          # produced in Milestone 13 (deployment docs; still pending)
  docker-compose.yml         # Postgres 16 for dev; Synapse under `integration` profile
  scripts/
    integration-test.sh      # end-to-end E2EE re-decryption test vs local Synapse
  .github/workflows/         # public repo: GitHub-hosted runners have free minutes, so most workflows trigger on push or release (via new tag); a few stay manual-dispatch only for other reasons (expensive/manual runs)
    api-docs.yml             # build the Pages site: homepage + API reference at https://matrix-axon.github.io/matrix-axon/api.html
    check-environment.yml    # local-runner environment check
    cross-build.yml          # fmt, clippy, test, and build for MacOS, Linux, and Windows (can manually select subset if desired)
    lint-and-clippy.yml      # cargo fmt + clippy (faster than lint-and-test)
    lint-and-test.yml        # cargo fmt + clippy + test
    integration.yml          # E2EE re-decryption test (Synapse + Postgres)
    smoke.yml                # S1 black-box smoke (PR 1: TUI PTY suite; PR 2 added the server gate)
  .github/actions/           # actions relied on by the workflows
    check-environment/       # check environment on local runners before proceeding
```

## Crates

Each crate's own `Cargo.toml` `description` is the source of truth;
this table is the orientation copy.

| Crate         | Purpose                                                                                                                           |
| ------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| `axon-core`   | Shared types, errors, and configuration.                                                                                          |
| `axon-store`  | Postgres-backed event store, room state, and account data.                                                                        |
| `axon-sync`   | matrix-rust-sdk sync-engine wrapper (Simplified Sliding Sync); owns verification and all crypto-adjacent logic.                   |
| `axon-crypto` | Reserved stub — stays empty; verification lives in `axon-sync` (ADR 0027; see the "`axon-crypto` stays a stub" convention below). |
| `axon-search` | Tantivy full-text search index, populated on event ingestion.                                                                     |
| `axon-media`  | Media proxy with a bounded on-disk LRU cache for `mxc://` URLs.                                                                   |
| `axon-api`    | axum HTTP + WebSocket handlers; OpenAPI spec via utoipa.                                                                          |
| `axon-server` | The `axon` binary — wires the crates together and owns the process.                                                               |
| `axon-itest`  | Dev-only end-to-end integration-test seeder (plays a verified Matrix client).                                                     |

## Key conventions

- **One human per Axon process.**
  N Matrix accounts inside, every account-scoped table carries `account_id`.
- **Clients:** client apps live under `clients/`.
  Follow any subtree `AGENTS.md` there;
  `clients/tui/AGENTS.md` covers axon-tui-specific conventions.
- **Sync:** Simplified Sliding Sync (MSC4186) only.
  No legacy `/sync`.
- **Event schema:** hybrid hot-columns + JSONB.
  `origin_ts` is `BIGINT` milliseconds since Unix epoch.
- **Provenance:** every event row has `provenance = 'upstream_homeserver'` for MVP.
- **API:** all routes under `/v1/`.
  WebSocket at `/v1/ws`.
  Envelope `{type, account_id, payload}`.
- **Migrations:** under `crates/axon-store/migrations/`, UTC timestamp prefixes (`YYYYMMDDHHMMSS_description.sql`, via `sqlx migrate add`) to avoid cross-branch collisions;
  forward-only — see ADR 0004.
  For any table that carries `updated_at TIMESTAMPTZ`, add a `BEFORE UPDATE` trigger (using a shared `trigger_set_updated_at()` plpgsql function) so application queries never need to remember `updated_at = now()` — the DB enforces it automatically.
- **Invited-room visibility (ADR 0091).**
  Pending invites are a dedicated `room_invites` projection (not `events` / `room_state` / `GET /v1/rooms` — stripped invite state has no event id).
  `axon-sync`'s `watch_invites` captures `client.invited_rooms()` the same way ADR 0070 captures unread counts, prunes on positive absence (or a non-empty invited list) or a 404 from a leave/reject attempt classified as `LeaveOutcome::Gone`, and fans out `invite.added` / `invite.removed`.
  `GET /v1/invites` is the reconnect source of truth.
  Accept is existing `POST …/rooms/join`;
  reject is existing `POST …/rooms/{id}/leave`.
  Known-contact DM auto-join (ADR 0040) is unchanged.
  `axon-web` consumes this (Invites row + `/invites` inbox);
  `axon-tui` does not — it has no inbox and is the outstanding consumer, tracked in `docs/client-parity.md`.
- **A migration that has landed on `main` is immutable — never edit it, not even a comment.** sqlx checksums each migration file's _entire contents_ into `_sqlx_migrations` and refuses to start when the bytes no longer match, so a comment-only or whitespace edit is a hard startup failure (`migration <version> was previously applied but has been modified`) on every database that already applied it — including every deployed instance, which is exactly who never re-reads the file.
  To change schema, add a new timestamped migration;
  to correct or expand a comment, say it in the new migration that supersedes it rather than editing the old one.
  `scripts/check-migrations-immutable.sh` enforces this.
  It runs in CI from `migrations-immutable.yml`, which is `pull_request`-triggered on purpose: only that event supplies the canonical base (`base.sha`) without a credential the fork doesn't have.
  It also runs from the pre-push hook (`migrations-immutable` in `.pre-commit-config.yaml`), which covers branch pushes before a PR exists.
  It compares against the **canonical** `main` — not your fork's, which lags, so `git fetch upstream main` before trusting a clean local result — and only attributes changes made by commits this branch adds, so inheriting a migration change from `main` is never reported as yours.
  Only files that already exist on that base count as published — a migration your own branch adds is not in anyone's database yet, so amending it in a follow-up commit before the PR merges is fine.
  It also checks uncommitted work, so you can run it by hand at any point — though not through the hook, where `pre-commit` stashes unstaged changes first and the script therefore sees only committed state (the right semantics for a push gate).
  A commit that must change a published migration anyway (reverting a bad edit back to the bytes deployments already applied, as `#375` did) declares itself with a `Migration-edit-approved: <reason>` trailer in its message;
  the check then reports the change and passes, and the PR should justify it.
  If a database is already broken this way, `axon db repair-migrations --apply` (`--features dev-tools`, CONTRIBUTING.md §Troubleshooting) rewrites the metadata for a checksum-only drift.
- **ADRs:** under `docs/adr/`, monotonic `NNNN-kebab-title.md`.
  Numbers collide across branches just like migrations do — before claiming the next number, check the open branches/PRs (`git ls-tree -r origin/<branch> docs/adr/`, or scan the open PRs), not just `main`.
  If two land on the same number, the later-merged one renumbers.
- **Errors:** `thiserror` in libraries;
  `anyhow` only at the `axon-server` binary boundary.
- **Logging:** `tracing` with structured fields — always include `account_id`, `room_id`, `event_id` where applicable.
  Logging and debugging support should always be considered as part of the design of new features or functionality.
  Before finalizing a new or modified feature, consider whether there is adequate logging and also whether any "developer mode" configurable option should be added to allow debugging in the future.
- **OpenAPI:** the spec is the source of truth.
  Handler types must compile against it (utoipa).
  Drift between spec and generated stubs is a bug.
- **Component separation:** the code is separated into three silos: `crates/` (server-side infrastructure), `clients/` (client-side infrastructure, separated into `tui` and `web`), and `smoke/` (testing infrastructure).
  Commits and pull requests should **not** change multiple silos at once.
  Each PR should be limited to its own silo.
  Files can be added to `docs/` combined with other silos where they are directly related to the change in that silo.
  Typically, changes to the `web` and `tui` clients should be separate PRs except where a global change impacts both clients.
  The user can override this rule, but the agent should never do this on its own.
- **`axon-crypto` stays a stub.**
  Verification and all crypto-adjacent logic live in `axon-sync` — the engine can't be thin, it needs `ClientManager`, the per-identity locks, and supervision (ADR 0027).
  Do not add logic to `axon-crypto`;
  it remains a reserved stub.
- **Pull requests:** every PR body includes, by default, a **Verification guide** (prereqs + copy-pasteable, end-to-end steps that exercise real behavior — not just `cargo check`) and a **Code review guide** (a suggested file-by-file review order, dependencies first, plus a "where to keep a close eye" section calling out correctness, security, and lifetime concerns).
  Match the format of PRs #6 and #7.
  Scope both guides to the PR's actual diff.
  Subsequent changes to code in PR's should be impelmented as additional commits and pushes so that responsive comments can identify the precise commit that addresses the issue.
  Generally, the commits in a PR will all be squashed at merge time, subject to developer approval.
- **Pushes:** we normally do not push anything directly to `main`.
  Code is incorporated into `main` when PRs are merged.
  Any exception should be approved by a developer. git force-push should only be used to fix an error in branch history and not without developer approval.
- **Stacked PRs and branch deletion:** GitHub **silently closes** any open PR whose _base_ branch is deleted out-of-band (`git push --delete`, `jj git push --deleted`, remote pruning).
  Only deleting the branch through GitHub's own "Delete branch" button on the merged PR retargets stacked children to the merged PR's base instead of closing them.
  So: before deleting a merged PR's branch, check for open PRs based on it (`gh pr list --base <branch>`) and either retarget them first (`gh pr edit <n> --base main`) or delete the branch via the GitHub UI.
  This has already cost us real work once — a feature PR was auto-closed this way and went unnoticed until live testing rediscovered the gap it addressed — so treat a nonzero `gh pr list --base` result as a hard stop.
- **Restack dependent PRs when a base changes shared files.**
  When a base PR's review fixes touch files a stacked child also edits, rebase the child and resolve conflicts immediately — don't leave reviewers staring at GitHub's conflict state.
- **Measure the engine before working around it — and never mistake the automation driver for the engine.**
  A cross-browser e2e lane failing only under Firefox looked like a Firefox bug and was not one: Playwright's `page.reload()` is reported as `navigate` by Navigation Timing under Firefox, while _every_ engine including Firefox reports `reload` for an in-page `location.reload()` (measured on Firefox 151; Chromium and WebKit report `reload` for both).
  Nothing a user does reaches the driver's reload, so the browser needed no workaround at all — but before that was measured, this repo spent eight review rounds building production ones: user-agent sniffing, deprecated-API fallbacks, and a `sessionStorage` marker, each of which introduced its own bugs.
  So, in order: (1) before adding any compatibility shim for navigation, storage, or timing, **measure the signal in the target engine and paste the reading into the PR** — a claim about a browser is a measurement, not an inference from a failing test;
  (2) drive reload and history e2e from inside the page, because a driver-level `page.reload()`/`goBack()` is not guaranteed to be classified the way the user-initiated action is;
  (3) if a fallback really is warranted, enumerate and test every supported signal state — a modern value, no modern value, a throwing accessor, each legacy override — not just the case that motivated it, and preserve established fallback behavior unless the PR documents the compatibility change on purpose.
  Also keep the PR's verification and review guides synchronized with the branch's actual Playwright projects and final file layout before requesting re-review;
  a guide naming a deleted test, or a Playwright project the branch does not define, wasted a reviewer's time here more than once.
- **`#N` is a GitHub autolink — never use it for anything else.**
  GitHub renders `#<number>` (in PR/issue bodies, comments, and commit messages) as a link to the issue or pull request with that number.
  Only write `#` immediately before a number when you mean a link to that exact, existing issue or PR.
  For every other numbered thing — review-comment indices, milestone phases, list items, ordinals, counts, versions — omit the `#` (write "comment 4", "step 3", "milestone 7a", "v2") so prose never sprouts bogus cross-links to unrelated issues.
- **Robustness at boundaries.**
  Every place axon crosses a network or process boundary — a client calling `/v1/`, axon-server calling a homeserver, the TUI calling axon — is a "what could go wrong?" checkpoint.
  Before merging code that crosses one, account for: (1) **Timeouts** — every outbound call gets one;
  never `await` a remote unbounded.
  (2) **Bounded resources** — a fixed pool (workers, connections, semaphore permits) must be paired with timeouts and/or cancellation, so one slow/hung peer can't permanently consume it.
  (3) **Hostile input** — validate size and shape _before_ allocating from it;
  cap bodies/images/list lengths;
  never size a buffer or allocation directly from a number the peer controls.
  (4) **Concurrency** — name the shared mutable state and the lock/owner guarding it (the cold-connect gate vs. live-task severing in 7a is the model), and state which lost-update/reconnect race is closed and how.
  (5) **Partial failure** — one account/room/event failing is logged and skipped, never fatal to the loop (the established "best-effort, never fatal to sync" philosophy).
- **Secrets never reach logs, errors, or disk.**
  No log line, error message, or file may contain a password, access token, recovery key, or bearer token.
  The one sanctioned exception is a value the _developer_ explicitly surfaces for the end user to consume once — `axon token issue` printing the raw bearer token to stdout, or `axon init` emitting a generated secret — and even then only at the moment of issue, never re-logged afterward.
  Secret-bearing inputs (login password, 4S recovery key) are consumed once and dropped, never persisted (ADR 0008, 0026).
- **Crash-safe multistep flows.**
  Axon or a client can be killed, crash, or lose power at any point in a non-atomic flow.
  Any multistep flow must leave the system in a state it can reconcile on the next boot — never a wedged resting state that can't recover on resume.
  The `deleting` teardown breadcrumb + boot reconcile (7a-4) and the transactional `search_outbox` (9a) are the models: write a durable intent, make the completion idempotent, heal on resume.
- **No duplicate code.**
  Before landing duplicate or near-duplicate logic, extract a shared helper or refactor.
  Duplicated implementations drift apart, and each copy becomes a place a fix can be forgotten — the shared `TIMELINE_SELECT` projection, the config-discovery rules, and the TUI parsers are all single-source-of-truth for exactly this reason.
- **Docs track code.**
  A change to behavior that end-user docs (`README.md`), contributor setup (`CONTRIBUTING.md`), or agent instructions (`AGENTS.md`, subtree `AGENTS.md`s) describe must update those docs in the same PR — they must reflect the project's state once the PR merges.
  Stale docs are a review finding.
- **Break prose lines semantically, not at a column.**
  New markdown prose gets a newline after each sentence, or after a long clause — one sentence per line.
  Nothing enforces this and nothing reflows: `prettier` runs with `proseWrap: "preserve"`, so it normalizes tables, lists and emphasis and never touches a line break in body text.
  The reason is diffs.
  An unwrapped paragraph makes every edit one enormous changed line;
  a hard wrap at a column makes a one-word edit rewrap the whole paragraph, so a typo fix arrives as a ten-line diff.
  A break per sentence gives a diff that names the sentence that changed, and never rewraps.
  Column wrapping was measured and rejected: `proseWrap: "always"` on `README.md` rewrote 62 lines into 162 and _still_ left lines of 226, 218 and 194 characters, because tables and long links cannot be broken.
  This applies to prose you write, and to paragraphs you are already rewriting for other reasons.
  `AGENTS.md` and `README.md` are reflowed (#225);
  every other file is not, `CONTRIBUTING.md` included — 83% of its paragraph-continuation line breaks still fall mid-sentence at a column.
  Nothing else is being reflowed on sight, because a reformat mixed into an ordinary PR is a diff nobody can review and a blame nobody can skip.
  To reflow one, do it **alone, in its own PR** with no other change of any kind — see the note in `.git-blame-ignore-revs` for why a commit-level split cannot substitute for a separate PR.
  **That is two pull requests, not one.**
  A squash SHA does not exist until the squash does, so listing it is necessarily a second, later change: reflow PR → merge → a one-line follow-up adding the SHA.
  Skipping that follow-up leaves the reformat un-ignorable, which was the entire reason for isolating it.
- **A code span must not cross a newline** inside a list item or a blockquote.
  Whitespace inside `` `backticks` `` is content, so `prettier` cannot re-indent the continuation line and dedents it to column 0 instead — dropping the list indentation, or the blockquote's `>`.
  CommonMark's lazy continuation means it still renders correctly, so the damage never shows in the output and only shows in the source.
  Keep the span on one line, or shorten it.
- **OSS Rust conventions + simple dependencies.**
  Follow idiomatic open-source Rust unless there's a specific reason to deviate.
  Keep the dependency graph simple and prefer indirection (a consumer-owned port + composition-root adapter, ADR 0021) over tight coupling.
  When an ADR justifies a dependency choice against an alternative already in the graph, explain the _behavioral_ difference, not just a library preference.
- **What not to build:** no push (APNs/FCM), no admin API, no multi-human-per-process, no federation, no S3 media backend — see `docs/mvp/implementation.md` "What not to build" for the full list.
- **Spelling:** U.S. English throughout all source files, comments, and docs (e.g. "initialize" not "initialise", "honors" not "honours").
  Note that the Matrix spec itself uses some Britishisms (`m.tag` `favourite`), in which case the standard should be used in code.
- **Version control:** some developers who contribute to this repo use [jj (Jujutsu)](https://github.com/jj-vcs/jj) in colocated mode alongside git (both `.jj/` and `.git/` are present).
  _If the developer has jj in their local environment_, prefer jj commands for committing and branching;
  git commands still work but are not the primary workflow.
  Key operations:
  - Commit message: `jj describe -m "..."`
  - New commit on top of current: `jj new`
  - New commit off a specific base: `jj new <bookmark>@origin`
  - Switch working copy: `jj edit <change-id>`
  - Create/move a branch bookmark: `jj bookmark create <name> -r @` / `jj bookmark set <name> -r @`
  - Push: `jj git push --bookmark <name>`
  - Restack after base moves: `jj rebase -d <base-bookmark>` then re-push
  - PRs are still opened with `gh pr create --base <base> --head <branch>`;
    `gh`'s "uncommitted changes" warning in colocated mode can be ignored as long as the bookmark was pushed correctly.
- **Pre-push hook (strongly recommended).**
  `.pre-commit-config.yaml` is the **only** list of checks the repo runs before a push, for both front-ends (ADR 0092).
  It mirrors CI's web job (`web-lint-and-test.yml`) and rust fmt/clippy jobs (`lint-and-test.yml` / `lint-and-clippy.yml`), path-filtered so a web-only push does not run clippy and a rust-only push does not run `pnpm`, and it adds `cargo test --all` — which runs in **no** automatic CI job (`lint-and-test.yml` is `workflow_dispatch`-only, `lint-and-clippy.yml` has no test step), so skipping the hook means the suite has not run at all.
  The hook requires the [`pre-commit`](https://pre-commit.com) runner (`pipx install pre-commit`, `uv tool install pre-commit`, `pip install --user pre-commit`, or `sudo apt install pre-commit`);
  everything else it needs it installs itself.
  Enable it once per clone:
  - **git users:** `./scripts/setup-hooks.sh` — runs `pre-commit install --hook-type pre-push`, so the gate fires natively on `git push`.
    (If you have an older clone, run it again: it used to set `core.hooksPath = .githooks`, that directory is gone, and a stale `hooksPath` silently disables every hook without git saying so.)
  - **jj users:** git hooks don't fire under jj, so push through the `jj-hooks` tool (`cargo install jj-hooks`) instead: `jj-hooks --runner pre-commit push`.
    To make `jj push` do that by default, add this alias to `~/.config/jj/config.toml`:

    ```toml
    [aliases]
    push = ["util", "exec", "--", "jj-hooks", "--runner", "pre-commit", "push"]
    ```

    Use `jj push`, not `jj git push`;
    direct `jj git push` bypasses the alias.

  Both front-ends read the same `.pre-commit-config.yaml`;
  only the driver differs.
  On a failure, run the command printed by the hook (for example `pnpm --dir clients/web format` for formatting, `cargo fmt --all` for rustfmt, or `pnpm --dir clients/web gen:api` for schema drift) and push again.
  To skip deliberately: `SKIP=<hook-id> jj push` for one hook, `git push --no-verify` for all of them.
  CI is the backstop for fmt, clippy and the web job, but _not_ for `cargo test --all` — see above.

  **For what is in the gate, read `.pre-commit-config.yaml` itself** — it is the list, top to bottom in the order it runs, with each hook's command, path filter, and a comment on why it is there.
  Do not restate it here: a prose copy is a second list to keep in sync, which is the exact failure ADR 0092 exists to end.
  What belongs here is only what that file cannot say about itself: **`cargo test --all` is in it and in no automatic CI job**, and two things are deliberately _absent_ from it — Playwright (browsers and minutes; `web-e2e.yml` gates PRs, and `clients/web/AGENTS.md` says when to run it locally) and the Docker smoke lanes (opt in per push with `RUN_SMOKE=<lane>`).

- **Web verification uses the package scripts.**
  For `clients/web` code changes, the local gate is `pnpm --dir clients/web lint`, `pnpm --dir clients/web test`, and `pnpm --dir clients/web build`.
  Do not use plain `tsc --noEmit` as a substitute for the build typecheck: the web package's root `tsconfig.json` is a project-reference file with no direct inputs, so plain `tsc --noEmit` can check zero app files.
  Use `pnpm --dir clients/web build` (or, for TypeScript only, `pnpm --dir clients/web exec tsc -b`) so the referenced app/node projects are checked.

Full conventions are in `docs/mvp/implementation.md` under "Conventions."

## Bootstrap and configuration

`axon init` (M13, ADR 0051) is the first-run bootstrap — it owns starter-config generation, secret generation, and local-service setup.
Config precedence and discovery are described in the Milestone 2 notes below (figment: defaults < TOML < `DATABASE_URL` < `AXON_`-prefixed env).

- **Bootstrap has one owner.**
  `axon init` owns config/secret generation and first-run setup;
  helper and launcher scripts must not reimplement it.
  Scripts stay as source-checkout developer scaffolding.
- **Shell scripts must not reimplement config precedence.**
  Anything that exports `AXON_*` outranks the TOML file and can silently clobber user/platform config.
  Prefer the Rust config APIs over shell/PowerShell copies of the discovery rules.
- **Explicit config paths are promises.**
  `--config <path>` and `AXON_CONFIG=<path>` must fail loudly when the file is missing.
  Silent absence is acceptable _only_ for convention-based discovery (`./axon.toml`, the platform config dir).
- **"No config found" ≠ "config is broken."**
  First-run prompts fire only when no config file exists _and_ no config env is set.
  A parse error, a missing explicit path, or invalid env must fail with the real error, never silently drop into first-run.
- **Launcher behavior stays boring.**
  A run/launcher script does root-relative paths, `.env` loading, Cargo invocation, and optional dev Postgres — nothing more.
  No secret generation, config-file writing, or storage-location overrides unless the script's stated purpose _is_ bootstrap.
- **Fallbacks that look like data loss must be observable.**
  If platform data/config/cache dirs can't be resolved and axon falls back to CWD-relative paths (ADR 0050), log a warning — a silent fallback hides where state landed.

## Testing and documentation

- **Isolate platform-default state in tests and docs.**
  Any integration test, smoke guide, or manual repro that expects throwaway state must set `AXON_SYNC__DATA_DIR`, `AXON_SEARCH__INDEX_PATH`, and `AXON_MEDIA__CACHE_DIR` explicitly — otherwise it reads and writes the real platform dirs (ADR 0050) and leaks state between runs.
- **Document config.**
  Each silo must have documentation of configuration and examples.
  For the server, README.md should list the most common config options, and .env.example should include examples of every option that can be set via environmental variables.
  Update these files when config options are added or changed.

## Design guardrails

Recurring failure modes distilled from M1–M13 review.
They apply across crates and clients.

1. **Encode invariants in types, not comments.**
   Shared mutable state guarded only by a doc-comment is a latent bug — make the type system enforce it (the 7a per-identity lock and run-scoped verification tokens are the model, ADR 0022 / 0027).
2. **Store the key next to shared-but-partitioned state.**
   Whenever there is one physical slot with N logical owners, keep the owner's key beside the value — _or_ flush/commit the slot on every key change.
   A single global slot keyed by "current context" silently drops concurrent work when the context switches mid-operation (the PR 192 draft-compose case: a per-room compose buffer and its pending queue must carry the room key or flush on room switch, never live in one "current room" slot).
3. **Bracket every borrow of shared state.**
   A borrow should be save-on-enter / restore-on-exit — ideally structurally via a scope guard, not by remembering to add restore calls at each exit path.
4. **Never infer intent from an incidental value shape.**
   When multiple code paths can produce the same value, that value can't carry semantics on its own — use explicit sentinels (the `search_outbox` account-purge sentinel, 9a), not shape-guessing.
5. **Reconciliation must handle absence, not just presence.**
   Prune-on-reconcile is half the job of any lossy / eventually-consistent sync;
   a reconcile that only adds and never removes is incomplete (the deferred config-drop half of #24, ADR 0024).
6. **In a single-threaded loop, fan out independent I/O.**
   Don't serialize independent remote calls behind one another when they could run concurrently.
7. **User-entered text must survive a failed mutation.**
   Any operation built from typed input (send, edit, draft) must leave that input recoverable when the call fails — a retryable local echo, a restored composer, a preserved buffer — never error-banner-and-gone.
   (The web client's sends have retryable echoes but a failed edit silently discarded the user's text — WCR-10 in `docs/reviews/2026-07-web-client-review.md`.)
   The one exception stays: secret-bearing inputs are consumed once by design, per the secrets rule above.
8. **Every view of server state declares its freshness story.**
   A client store or panel that displays server data must answer, at design time: which live frames update it, what happens on reconnect, or why staleness is acceptable.
   "Fetched once on mount" is a decision to document, not a default to fall into — the client-side counterpart of guardrail 5's "reconciliation must handle absence".
   (From WCR-06/08: the web thread panel and room list each silently opted out of ADR 0061's subscribe + gap-fill pattern.)
9. **A predicate's tests do not cover the code that calls it.**
   Correctness that lives in a pure helper — a visibility filter, a status classifier, an error mapper — attracts exhaustive unit tests, while the branch consuming it typically gets none.
   So a call site that asks a _narrower_ question than the one it needs, or ignores the answer entirely, ships green with a fully-tested predicate sitting right beside it.
   Four instances in one review round (#134/#135/#165), across three silos: `unread_suppression_reason` was handed `successor_room().is_some()` where `Store::list_rooms` hides on any tombstone;
   the branch reading `probe_proves_room_absent` folded "we could not tell" into "reachable" while the predicate's own tests asserted `!probe_proves_room_absent(Some(500))`;
   the web read effect never called `isVisibleTimelineEvent`;
   the TUI room-load path filters its receipt pick through `should_show_event` and its marker pick, one line below, not at all (#167).
   When extracting a predicate, build the seam that lets its _consumer_ be tested too — a fake, a loopback server, a pure function taking the outcome — and prefer a type that makes the third state unrepresentable over a `bool` the call site can quietly collapse (guardrail 4, one level up).

**When implementing a new feature:**

- A feature that touches shared state implicitly interacts with _every_ existing path that touches that state.
  The audit surface is all readers/writers of the shared field, not just the diff.
- Test the transitions, not just the core operation.
  Boundaries and state transitions are where invariants break.

## Instructions after making changes

- Make sure to run `cargo fmt` on the code you modified after finishing making changes;
  limit the scope to the code you changed
- Make sure to fix any clippy issues `cargo clippy` on the code you modified after making changes
- Don't ignore any linting warning or add comments like "#[allow(clippy::too_many_arguments)]".
  The user can override a clippy warning but the agent should not do so on its own.
- **A docs-only commit may skip the full pre-push gate.**
  If a commit changes _only_ documentation (`docs/`, `AGENTS.md`, `CLAUDE.md`, `README.md`, `CONTRIBUTING.md`, subtree `AGENTS.md`s — and no source, config, migration, workflow, or CI file), the fmt/clippy/test gate exercises nothing in the diff, so pushing past the hook (`git push --no-verify`) is acceptable.
  Any commit that touches code stays subject to the full gate below.
- More complete formatting and clippy checks can run before committing/pushing
  **The repo is public, so GitHub-hosted runners (`ubuntu-latest`, `windows-latest`, `macos-latest`) get free, unlimited standard-runner minutes.**
  `lint-and-clippy.yml`, `cross-build.yml`, and `publish-images.yml` trigger automatically on push/tag;
  workflows stay `workflow_dispatch`-only only where that's the right call for other reasons (e.g. expensive/manual smoke or integration runs), not because of a minutes budget.
  The pre-push hook is still the safety net for anything that isn't covered by an automatic run — and `cargo test --all` is covered by none of them, since `lint-and-test.yml` is `workflow_dispatch`-only and `lint-and-clippy.yml` has no test step.
  Before pushing, either install the hook once per clone with `./scripts/setup-hooks.sh` (git) or the `jj-hooks` alias above (jj), or, if you haven't, run it by hand:

```bash
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --all
```

The `tui` smoke lane runs in CI on every pull request (`smoke.yml`): it needs no Docker and takes about 90 seconds.
The Docker-backed lanes stay out of the PR path — they need a true-local Synapse stack — and run on push to `main` and nightly instead.
Use `scripts/smoke-gate.sh <mode>` before pushing component changes that cross process/network boundaries, so a Docker-lane break is caught before it lands rather than after:

| Changed area                                                                                                          | Smoke gate                             |
| --------------------------------------------------------------------------------------------------------------------- | -------------------------------------- |
| `smoke/tui` or terminal rendering in `clients/tui`                                                                    | `scripts/smoke-gate.sh tui`            |
| TUI changes that depend on real Axon/local-stack behavior                                                             | `scripts/smoke-gate.sh tui-true-local` |
| `smoke/server`, `crates/axon-server`, `crates/axon-api`, `crates/axon-sync`, or `crates/axon-store` API/sync behavior | `scripts/smoke-gate.sh server`         |
| `smoke/local-stack` or shared smoke harness behavior                                                                  | `scripts/smoke-gate.sh all`            |

The pre-push hook does not run Docker-backed smoke by default.
Opt in with `RUN_SMOKE=tui`, `RUN_SMOKE=server`, `RUN_SMOKE=tui-true-local`, or `RUN_SMOKE=all` — the `smoke-gate` hook is a no-op unless that variable names a lane, and works the same under `git push` and `jj push`.

## Current state

**Through M19 landed;
MVP has not shipped.**
MVP is gated on finishing M13 — `docs/self-hosting.md` and the non-Docker deployment recipes (#221).

What each milestone delivered, and the non-obvious choices made along the way, lives in [`docs/mvp/implementation.md`](docs/mvp/implementation.md) under that milestone's heading, with the reasoning in its ADR.
It is deliberately not repeated here: this file is the conventions those choices produced, not the record of making them.
