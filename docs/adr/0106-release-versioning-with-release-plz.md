# ADR 0106 — Release versions come from Cargo, bumped by release-plz

**Status:** Accepted.
Implemented in the build tooling alongside this record.

## Context

A release is named twice.
The git tag names the GitHub Release and everything `cross-build.yml`, `package.yml`, `desktop-build.yml` and `publish-images.yml` attach to it.
`[workspace.package] version` in `Cargo.toml` is `CARGO_PKG_VERSION`: what `axon-server --version` prints, and what `packaging/package.sh` stamps on the `.deb` and `.rpm`.

Nothing kept the two together.
The workspace version has been `0.1.0` since the initial commit, while tags ran from `v0.0.1` to `v0.0.16`.
Measured on the v0.0.16 Release: its `axon-server-linux.zip` prints `axon-server 0.1.0 git_hash="992e06e" …`, and its Linux packages are versioned 0.1.0 too.
The Homebrew formula (PR 484) found this the hard way: a `brew test` asserting that the binary reports the formula's version fails against every release so far, and had to be loosened to a shape check.

The desktop app already solved its half of this: `.github/actions/tauri-build` rewrites `clients/web/package.json`'s version from a `v<semver>` tag before building (ADR 0087, amended).
Nothing did the same for the Rust binaries.

## Decision

### The Cargo version is the release version, and the tag must equal it

`scripts/release-version.sh check` fails a `v*` tag that is not `v<axon-server's Cargo version>`.
It runs first in `cross-build.yml`'s `generate-thirdparty` job, which every build job needs, and before the build in `package.yml`'s `packages` job.
A mismatched tag therefore stops before anything is built or attached to a Release.
`beta-*` and `alpha-*` tags pass untouched: they name builds, not versions, and the Homebrew tap already ignores them.

The version is read with `cargo metadata`, not a grep of `Cargo.toml`, so it is resolved the way the build resolves it.
`packaging/package.sh` reads it through the same script, so the package version and the guard cannot disagree.

The direction is deliberate.
Stamping the tag into the binary at build time, as the desktop action does, would also make them agree, but would leave `Cargo.toml` permanently wrong for every local and untagged build, and would need its own plumbing in `build.rs`.
Making the committed version authoritative means a checkout at a tag reports that tag with no CI involved.

### release-plz bumps the version and cuts the tag

`.github/workflows/release-plz.yml` runs [release-plz](https://release-plz.dev) on every push to `main`:

- `release-pr` opens, or updates, one "release vX.Y.Z" PR that bumps `[workspace.package] version` and `Cargo.lock`.
  The next version comes from the commit messages since the last `v*` tag, conventional-commit style: in 0.x, `fix:` and `feat:` bump the patch and a breaking change bumps the minor.
- `release` tags the merge of that PR as `vX.Y.Z`, and does nothing on any other push (`release_always = false`).
- `release-pr` runs after `release`, never beside it.
  On a release PR's merge it has to see the tag `release` just created; when the two ran in parallel, `v0.1.1`'s merge compared against `v0.1.0` and opened an empty `v0.1.2` release PR.

That tag is the only thing a release now needs a person for: review and merge a PR.

Configuration (`release-plz.toml`):

- `git_only = true`: nothing here is on crates.io, so the last release is read from the `v*` git tags rather than a registry.
- `git_tag_name = "v{{ version }}"`, the existing tag format.
- Every crate inherits the workspace version, so only `axon-server` creates the tag; with tagging on for all fourteen, every crate would try to create the same one.
- `git_release_enable = false`: the tag workflows already create and fill the GitHub Release, and a second writer would race them.
- `changelog_update = false`: commit subjects here are only partly conventional, so a generated changelog would be mostly noise. ADR 0108 generates the notes and `CHANGELOG.md` from PR labels instead.
- `semver_check = false`: these are binaries; there is no library API for cargo-semver-checks to compare.
- Crates that ship in no artifact (`axon-itest`, `axon-test-support`, the four `axon-smoke-*`) get `release = false` and a tag template no tag uses, `{{ package }}-v{{ version }}`.
  See "New crates" below for why the template is the part that matters.

### New crates

In `git_only` mode release-plz resolves every workspace crate at the last tag matching that crate's template, whether or not the crate is released.
Under the shared `v{{ version }}`, a crate added after the last tag still matches it, is absent from that tag's checkout, and the whole run aborts.
This broke `main` the first time it happened: `axon-test-support` landed after `v0.1.0`, and `release-pr` failed with `cannot find package "axon-test-support" in workspace` (release-plz 0.3.169, `process_git_only_package` in `next_ver.rs`).
`release = false` alone does not avoid it; a template that matches no tag does, because a crate with no matching tag is treated as a first release instead.

So every new test or smoke crate goes into `release-plz.toml` with that template, in the PR that creates it.

A new crate that does ship hits the same failure until a release tag includes it, and there is no verified workaround yet.
Inverting the list, so that crates are exempt by default and shipped ones opt in, was measured and rejected: a change confined to a crate that is not opted in then opens no release PR at all, so a forgotten entry would silently hold changes back instead of failing loudly.

Dry-run against `main` at the time of writing: release-plz found `v0.0.16` as every crate's last release, took the committed `0.1.0` as already bumped, and after one more commit proposed `0.1.0 → 0.1.1` across all fourteen crates and `Cargo.lock`; a release dry run then planned exactly one tag, `v0.1.1`, from `axon-server`.

### The token

GitHub starts no workflow for a tag or a PR created with the default `GITHUB_TOKEN`.
With it, release-plz's tag would build nothing, and its release PR would get none of the required checks.
Both jobs use `RELEASE_PLZ_TOKEN`, a fine-grained PAT on this repository with Contents and Pull requests read/write, and neither runs when it is missing: they both need a `token` job that fails with an explicit error.
A GitHub App would remove the dependency on one person's account; it is more setup and can replace the PAT later without changing anything else here.

### The first release

The committed version was already `0.1.0`, with no `v0.1.0` tag, so release-plz would have treated it as already bumped and opened its first PR as `0.1.1`.
`v0.1.0` was pushed by hand instead, on this change's own branch commit before merge; the guard passed it in `cross-build.yml` and `package.yml`, since Cargo says `0.1.0`.

A squash merge leaves that tag outside `main`'s history.
Simulated beforehand: with the branch squashed onto `main` and `v0.1.0` not an ancestor, release-plz still found `v0.1.0` by name as the last release, reported every crate up to date, and proposed `0.1.1` after one more `fix:` commit.
So the first release PR appears after the next commit to `main`, and releases go through it from then on.

## Consequences

- Hand-made `v*` tags still work, but only on a commit whose Cargo version matches; the guard's error says how to get there.
- `main` carries a standing release PR whenever unreleased commits exist. It is updated in place, not reopened, on every push.
- The Homebrew formula's `brew test` can assert the exact version again once the first release-plz release has shipped.
- `desktop-build.yml` is not guarded. Its version already comes from the tag, and `clients/web/package.json` is in the client silo; making its committed value follow the workspace version is a separate change. That change has since been made: `scripts/bundle-version.sh` keeps `package.json` at the workspace version, the release PR carries it, and the desktop, store and Android builds all take it from there rather than from the tag.
- A tag release-plz creates is authored by whoever owns `RELEASE_PLZ_TOKEN`.
