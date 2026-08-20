# ADR 0092 — One pre-push gate for git and jj

## Context

The repo carries two local hook systems that do not share a driver, so which
checks a developer runs depends on which VCS front-end they push with.

- `.githooks/pre-commit` and `.githooks/pre-push`, enabled by
  `scripts/setup-hooks.sh` via `core.hooksPath`. Between them: actionlint,
  `scripts/check-migrations-immutable.sh`, `cargo fmt --all -- --check`,
  `cargo clippy --all-features --all-targets -- -D warnings`, `cargo test --all`,
  and the `RUN_SMOKE=` opt-in. These fire for **git only**.
- `.pre-commit-config.yaml`, read by the `pre-commit` runner. This is what
  **jj** pushes execute, through `jj-hooks` (git hooks do not fire under jj), and
  what git users get from `pre-commit install --hook-type pre-push`. It carries
  prettier, the five web checks, and — since #173 — rustfmt and clippy.

The two cannot be installed together. `pre-commit install` refuses while
`core.hooksPath` is set ("Cowardly refusing to install hooks with core.hooksPath
set"), and running `setup-hooks.sh` afterwards orphans the pre-commit hook by
taking `hooksPath` over. So every clone runs one of two different lists, and the
lists have drifted. #173 was the first patch for that drift — it added rustfmt
and clippy to the shared file because a rust-only `jj push` had been skipping
rustfmt entirely — but it deliberately fixed only the symptom.

Three consequences of the split, each verified against this tree:

1. **`cargo test --all` runs in no automatic CI job.** `lint-and-test.yml` is
   `workflow_dispatch`-only; `lint-and-clippy.yml`, the one that fires on every
   push, has no test step; `cross-build.yml` runs tests but only on a version
   tag. On a normal pull request the *only* thing that runs the suite is
   `.githooks/pre-push` — precisely the hook a jj user never fires.
2. **`web-lint`, `web-test`, and `web-build` never fire** (#180). Their `files`
   regex is `^(…|clients/web/|…)$`, and `clients/web/` inside a `$`-anchored
   group matches only the literal string `clients/web/`, never a real path like
   `clients/web/src/app.tsx`. Those three hooks have only ever run when the
   workflow file or `.pre-commit-config.yaml` itself changed, which is not what
   the file's own header comment claims.
3. **actionlint and the migrations-immutable check are git-only.** actionlint
   additionally runs in no workflow at all, and both `.githooks/` copies guard it
   with `command -v actionlint &&`, so a machine without the binary silently
   runs nothing and a malformed workflow file reaches `main` unlinted.

Separately, the rust hooks' path filter (`crates/|clients/tui/|smoke/`) is
correctly unanchored — it does not have the #180 bug — but it matches *any* file
under those trees. Editing `crates/axon-store/README.md`, or a binary fixture
under `smoke/local-stack/corpus/`, buys a full
`cargo clippy --all-features --all-targets`: minutes, for a diff that cannot
change a single compilation unit. That cuts against the header comment's framing
of these hooks as cheap and path-filtered.

## Decision

**One list of checks, one driver, one install command.**

`.pre-commit-config.yaml` becomes the sole enumeration of what must pass before
a push. `.githooks/` is deleted. `pre-commit` is the only driver: git invokes it
natively through `.git/hooks/pre-push`, and jj invokes it through `jj-hooks`,
both reading the same file. `scripts/setup-hooks.sh` becomes the single install
path — it clears the `core.hooksPath` it used to set (only when that value is
still `.githooks`, so a genuinely custom hooksPath is left alone) and then runs
`pre-commit install --hook-type pre-push`.

Everything that lived only in `.githooks/` moves into the shared file, including
`cargo test --all`. That is a change of position from #172's original sketch,
which proposed leaving the expensive gate in a git-only wrapper. Two facts
overrode it: the suite runs in no automatic CI job, so leaving it git-only means
jj branches are pushed with nothing having run the tests; and any check that
exists in one driver and not the other reconstitutes the two-list problem this
ADR exists to end. `SKIP=cargo-test` (or `--no-verify`) remains the escape for
the rare case, and it is an explicit, visible one.

Two checks that CI runs and no hook mirrored are added at the same time: the
duplicate-ADR-number check, extracted from `lint-and-clippy.yml`'s inline bash
into `scripts/check-adr-numbers.sh` so the workflow and the hook call the same
script, and `pnpm peers check`, which is added to both the hook and
`web-lint-and-test.yml` so the local gate and CI cannot disagree about it.
actionlint moves the other way — it gains a CI step in `lint-and-clippy.yml`, so
it is no longer a check that exists only on a developer's machine, and the hook
uses the vendored upstream mirror rather than a soft-skipped system binary.

The rust filters are narrowed to paths that can actually change a rust build:
`.rs`, `.toml` and `.sql` under the workspace trees, the root manifests and
toolchain files, and `tests/fixtures/`.

Establishing that list is the part worth doing carefully, and grepping for the
obvious macro names is not enough. There are exactly two compile-time file
inputs here, and only one of them is an `include_str!`:

- `tests/fixtures/decryption/*.json`, pulled in by `axon-sync`'s `redecrypt.rs`
  and `backfill.rs`. These sit outside `crates/`, so the *old* filter missed
  them — narrowing this filter also widened it.
- `crates/axon-store/migrations/*.sql`, pulled in by
  `crates/axon-store/src/migrations.rs` via **`include_dir!`**, not
  `sqlx::migrate!` — the umbrella `sqlx` crate would drag `sqlx-sqlite` in and
  collide with matrix-sdk's rusqlite, so the migrator is built by hand. A
  first draft of this ADR asserted there were no compile-time inputs beyond the
  fixtures, on the strength of a grep for `include_str!`/`include_bytes!`/
  `migrate!`. `include_dir!` is none of those. Editing a `.sql` file changes the
  embedded checksums that `axon-server`'s `db.rs` reads and that
  `axon-store`'s tests assert on, so it must run clippy and the suite.

Beyond those: no `sqlx::query!` macros, no `.sqlx/` offline data, and both
`build.rs` files (`axon-server`, `axon-tui`) read only environment variables and
`git`/`jj` output.

`RUN_SMOKE=` survives as a hook whose entry is a no-op unless the variable is
set, so ADR 0086's opt-in reaches the shared gate rather than needing a second
mechanism. Playwright stays out of the hook — it needs browsers and minutes, and
`web-e2e.yml` already gates pull requests — but the expectation that a UI change
runs it locally is written down in `clients/web/AGENTS.md`, where it previously
was not.

### Rejected: `.githooks/pre-push` delegating to `pre-commit hook-impl`

#172 sketched keeping `.githooks/pre-push` and having it call
`pre-commit run --hook-stage pre-push` before running `cargo test --all` itself.
This works — `pre-commit hook-impl --hook-type=pre-push` consumes git's stdin ref
lines and computes the changed-file range correctly, so the delegation is only a
few lines. It was rejected because it leaves two files, two install paths, and a
check that one driver runs and the other does not. That is the state this ADR is
trying to leave, described more politely.

## Consequences

- **Nothing runs at commit time any more.** `.githooks/pre-commit` ran fmt and
  clippy on every `git commit`; that is gone. This is intended — the title of
  #172 is "one **pre-push** source of truth" — and jj has no commit hook to lose
  in the first place.
- **jj pushes now pay for `cargo test --all`.** That is the point, and it is the
  single largest behavior change for a jj user. `SKIP=cargo-test jj push` is the
  documented escape.
- **Existing git clones lose their gate silently until they re-run
  `./scripts/setup-hooks.sh`.** A clone that ran the old script still has
  `core.hooksPath = .githooks` pointing at a directory this change deletes; git
  runs no hooks and reports nothing. `lint-and-clippy.yml` still gates fmt and
  clippy on every push in the meantime, so the exposure is `cargo test --all` and
  the migrations check (the latter also runs in CI on every pull request). This
  needs to be called out in the pull request and in `AGENTS.md`.
- **`check-migrations-immutable.sh`'s uncommitted-work arm becomes a no-op under
  the hook.** `pre-commit` stashes unstaged changes before running, so the script
  sees only committed state. That is the correct semantics for a push gate — a
  push carries commits, not a working tree — and the script still reports
  uncommitted work when run by hand.
- **Narrowing the rust filters trades a little safety for a lot of time.** A
  compile-time input outside `.rs`/`.toml`/`.sql`/`tests/fixtures/` would no
  longer trigger clippy. None exists today; adding one means extending the filter
  in the same commit, and CI remains the backstop. `scripts/check-hook-filters.py`
  is what makes that survivable — it pins the intended selection for each of
  these paths, so a filter edited without its case updated fails loudly.
- **The docs point at the config instead of restating it.** `AGENTS.md` and
  `CONTRIBUTING.md` describe what the gate is *for*, what is deliberately absent
  from it, and how to skip it — but they do not enumerate the hooks, because a
  prose list is a second copy, and this ADR is about not having those. The
  config is readable on its own terms: every hook carries its command, its path
  filter, and a comment saying why it is there.

  This was not the first attempt. An earlier draft of this change had both docs
  list every hook in order, with `check-hook-filters.py` asserting the three
  copies agreed. That worked, and the `AGENTS.md` copy had already gone stale
  once inside a single branch — but enforcing agreement between three copies is
  the same mistake as the two hook lists, one level up. Removing two of the
  copies is the fix; the check that survives is narrow and does not require the
  docs to be exhaustive: every `SKIP=<id>` example in the docs, the config
  header, and `setup-hooks.sh` must name a hook that exists. That is the one way
  prose can name a hook and be silently wrong — rename `cargo-test` and
  `SKIP=cargo-test` still looks like a working incantation while skipping
  nothing.
- **The hooks now install web dependencies.** A `web-install` hook runs
  `pnpm install --frozen-lockfile` when the manifest, lockfile, or workspace file
  changes, which removes the standing footgun that the web hooks silently tested
  a stale `node_modules` (`clients/web/scripts/check-api-schema.mjs` already
  emits a remediation string for exactly that failure). It is the one hook with a
  side effect on the working tree, and `--frozen-lockfile` bounds it: it can
  never rewrite the lockfile.
