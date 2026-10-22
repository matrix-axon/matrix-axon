# Contributing to Axon

Everything you need between `git clone` and a merged pull request.
For what Axon _is_, see [README.md](README.md);
for working conventions and the current state of each milestone, see [AGENTS.md](AGENTS.md).

Building and running Axon from a checkout (Rust, Node, Postgres, platform install commands, `run.sh`, troubleshooting) is in [docs/building.md](docs/building.md).
This page covers what is specific to contributing.

## Prerequisites

Besides what [docs/building.md](docs/building.md) lists for the parts you will touch, you need:

- **[`pre-commit`](https://pre-commit.com)**, which drives the pre-push gate.
  `pipx install pre-commit`, `uv tool install pre-commit`, `pip install --user pre-commit`, or `sudo apt install pre-commit`.
  `./scripts/setup-hooks.sh` refuses to run without it.
- **Node.js and pnpm** if you touch `clients/web/` — or `.pre-commit-config.yaml`, which the web hooks also watch.
- **Docker** for `scripts/smoke-gate.sh` (except the `tui` lane) and for `scripts/integration-test.sh`, which brings up Postgres and Synapse via Compose.

**Optional but recommended:**

- **[jj](https://jj-vcs.github.io/jj/) (Jujutsu)** — the repo is jj-colocated and most work here is done with jj rather than raw git.
  Git works fine;
  if you use jj, you also want `cargo install jj-hooks` so `jj push` runs the same gate git gets (see [AGENTS.md](AGENTS.md) for the alias).
- **Playwright browsers**, for the web end-to-end suite: `pnpm --dir clients/web exec playwright install`.
- **[shellcheck](https://www.shellcheck.net/)** (`sudo apt install shellcheck`, `brew install shellcheck`).
  The `actionlint` hook vendors its own binary, but it shells out to shellcheck for the `run:` blocks inside workflows and **silently skips that analysis when shellcheck is missing** — no warning, just a weaker check.
  GitHub runners ship it, so CI always runs the strict version;
  without it locally, the hook can pass on a workflow CI will reject.

## First-time setup

### Install the pre-push gate

```bash
./scripts/setup-hooks.sh
```

Once per clone.
See [What runs before a push](#what-runs-before-a-push).
Then follow [First-time setup in docs/building.md](docs/building.md#first-time-setup) for Postgres and a config.

## What runs before a push

`.pre-commit-config.yaml` is the **only** list of checks the repo runs before a push, for both VCS front-ends (see [ADR 0092](docs/adr/0092-unified-pre-push-gate.md)).
`./scripts/setup-hooks.sh` installs it for git;
jj users get the identical list through the `jj-hooks` alias in [AGENTS.md](AGENTS.md).

It is path-filtered, so a web-only push does not build rust and a rust-only push does not run pnpm.

**Read `.pre-commit-config.yaml` for what the gate contains.**
It lists the hooks top to bottom in the order they run, each with its command, its path filter, and a comment saying why it is there — so this page does not repeat them.
Two copies of one list is the problem ADR 0092 exists to fix;
a copy here would go stale the first time a hook is added.

The required `lint-and-test` pull-request job runs `cargo test --all` after formatting and clippy.
The local hook runs the same suite so failures are caught before review begins; skipping that hook no longer skips the required CI copy.
CI also gates the whole web job on web pull requests.

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
  Behavior described by `README.md`, the guides under `docs/` ([index](docs/README.md)), `AGENTS.md`, or this file must be updated alongside the change.
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

## Releasing

Releases go through a pull request, not a hand-pushed tag (ADR 0106).
On every push to `main`, `release-plz.yml` opens or updates one "release vX.Y.Z" PR that bumps `[workspace.package] version` and `Cargo.lock`.
Merging it tags the merge commit `vX.Y.Z`, and that tag starts the release builds.
The next version is read from commit subjects since the last tag: `fix:` and `feat:` bump the patch while the version is 0.x, and a breaking change (`feat!:`, or a `BREAKING CHANGE:` footer) bumps the minor.

A `v*` tag pushed by hand must equal `v` plus the Cargo version, or `cross-build.yml` and `package.yml` stop before building anything.
Check one before pushing it with `scripts/release-version.sh check vX.Y.Z`.
`beta-*` and `alpha-*` tags are not checked.

Release notes are generated from the PRs merged since the last tag, grouped by PR label (ADR 0108, `.github/release.yml`).
Label each PR before merging: `breaking-change`, `security`, `enhancement`, `bug`, `performance`, `documentation`, `dependencies`, `build` or `ci`.
An unlabelled PR is listed under "Other Changes", and `skip-changelog` leaves a PR out.
While the release PR is open, `release-notes.yml` keeps a draft GitHub Release with those notes; edit it before you merge the release PR, and it is not overwritten once edited.
The tag publishes the draft, and a follow-up PR adds the notes to `CHANGELOG.md`.

A PR that adds a workspace crate which ships in no release (a test or smoke crate) must also add it to `release-plz.toml`, next to `axon-test-support`.
Without that entry, release-plz fails on `main` until the next release tag includes the crate.
