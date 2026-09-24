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

That tag is the only thing a release now needs a person for: review and merge a PR.

Configuration (`release-plz.toml`):

- `git_only = true`: nothing here is on crates.io, so the last release is read from the `v*` git tags rather than a registry.
- `git_tag_name = "v{{ version }}"`, the existing tag format.
- Every crate inherits the workspace version, so only `axon-server` creates the tag; with tagging on for all fourteen, every crate would try to create the same one.
- `git_release_enable = false`: the tag workflows already create and fill the GitHub Release, and a second writer would race them.
- `changelog_update = false`: commit subjects here are only partly conventional, so a generated changelog would be mostly noise. Revisit if they become consistent.
- `semver_check = false`: these are binaries; there is no library API for cargo-semver-checks to compare.

Dry-run against `main` at the time of writing: release-plz found `v0.0.16` as every crate's last release, took the committed `0.1.0` as already bumped, and after one more commit proposed `0.1.0 → 0.1.1` across all fourteen crates and `Cargo.lock`; a release dry run then planned exactly one tag, `v0.1.1`, from `axon-server`.

### The token

GitHub starts no workflow for a tag or a PR created with the default `GITHUB_TOKEN`.
With it, release-plz's tag would build nothing, and its release PR would get none of the required checks.
Both jobs use `RELEASE_PLZ_TOKEN`, a fine-grained PAT on this repository with Contents and Pull requests read/write, and fail at their first step when it is missing.
A GitHub App would remove the dependency on one person's account; it is more setup and can replace the PAT later without changing anything else here.

### The first release

The committed version is `0.1.0` and no `v0.1.0` tag exists.
release-plz treats `0.1.0` as already bumped, so its first PR proposes `0.1.1`.
To make `v0.1.0` the first release instead, push that tag by hand on the commit that merges this; the guard accepts it, since Cargo says `0.1.0`.
After that, releases go through the release PR.

## Consequences

- Hand-made `v*` tags still work, but only on a commit whose Cargo version matches; the guard's error says how to get there.
- `main` carries a standing release PR whenever unreleased commits exist. It is updated in place, not reopened, on every push.
- The Homebrew formula's `brew test` can assert the exact version again once the first release-plz release has shipped.
- `desktop-build.yml` is not guarded. Its version already comes from the tag, and `clients/web/package.json` is in the client silo; making its committed value follow the workspace version is a separate change.
- A tag release-plz creates is authored by whoever owns `RELEASE_PLZ_TOKEN`.
