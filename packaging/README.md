# Native packages

Debian and RPM packages via nFPM, and a Homebrew formula for `axon-server`.
Docker Compose remains the path that includes Postgres and the web client.
`docs/self-hosting.md` is a separate docs change.

## Debian and RPM

nFPM metadata for `axon-server` `.deb` and `.rpm`.
Postgres is recommended locally, not bundled.

## Build

```sh
cargo build --release -p axon-server --bin axon-server
# https://github.com/goreleaser/nfpm/releases
packaging/package.sh
```

Artifacts land in `target/nfpm/`.
Set `ARCH=arm64` for aarch64 packages.

CI (`.github/workflows/package.yml`) builds amd64 on `ubuntu-22.04` and arm64 on
`ubuntu-22.04-arm` so Bookworm/Jammy can run the glibc-2.35 binary.
On `v*` / `beta-*` / `alpha-*` tags it attaches the `.deb` and `.rpm` files to
the GitHub Release (alongside the zip archives from `cross-build.yml`).
The `.deb` version is `Cargo.toml`'s `version`, not the git tag.

## First install (local Postgres)

`apt install ./axon-server_*.deb` (or the rpm equivalent) with a reachable local cluster:

1. Creates Unix user `axon`.
2. Creates role `axon`, database `axon`, and `pgcrypto` as the `postgres` superuser.
3. Runs `axon-server init` as `axon` with a sqlx Unix-socket URL
   (`postgres://axon@%2Fvar%2Frun%2Fpostgresql/axon`, peer, no password).
4. Enables and starts `axon-server.service`.
5. Arms web bootstrap (`AXON_SERVER__BOOTSTRAP_WEB_AUTO`). URL is in `journalctl -u axon-server`.

Remote Postgres: the package still installs; see `README.Debian` in the doc directory of the built package.

## Homebrew

The formula template is `packaging/homebrew/axon-server.rb.tmpl`.
`packaging/homebrew/render-formula.sh` fills the version and the sha256 of the GitHub Release zips.
On a `v*` / `beta-*` / `alpha-*` tag, `cross-build.yml` publishes that formula to `matrix-axon/homebrew-tap` after the macOS and Linux zips are on the Release.

```sh
brew install matrix-axon/tap/axon-server
```

`matrix-axon/tap` is the repository `matrix-axon/homebrew-tap`.
Create that public repository once before the first tag that should update it, and store a fine-grained PAT as the `HOMEBREW_TAP_TOKEN` secret on `matrix-axon/matrix-axon` (contents write on the tap repository only).
Locally the same value is `TAP_TOKEN`.
The push sends that token alone, as the git HTTPS password.
It does not also use the GitHub credential helper in `~/.gitconfig`, because GitHub rejects the request when both are present.
A bearer header is not used: GitHub's git endpoint rejects one for the OAuth tokens `gh auth token` prints.
Until both exist, the tag job fails on purpose instead of reporting a formula it did not push.

The formula is a user service: `brew services start axon-server`.
`sudo brew services` reads root's home.
The formula sets no `AXON_CONFIG` and no `AXON_*` variable, so the service and `axon-server init` share platform discovery (ADR 0050).
On macOS that config is `~/Library/Application Support/axon-server/config.toml`.
On Linux it is `~/.config/axon-server/config.toml`.
Sync state, the search index, and the media cache stay in those platform directories.

PostgreSQL 16 is recommended.
Homebrew no longer has a `:recommended` dependency, so the formula leaves the install to you and a remote database still works.
Stock Homebrew Postgres creates neither role `axon`, nor database `axon`, nor `pgcrypto`, and `postgresql@16` is keg-only.
The caveats print `psql` / `createdb` against `$(brew --prefix postgresql@16)`.
init's default URL `postgres://axon:axon@127.0.0.1:5432/axon` works only after those exist.
Tailscale is a caveat too: `tailscale serve --bg http://127.0.0.1:8080`.
The menu-bar app is the common case; on a headless Mac mini, `brew install tailscale` is the formula, and the cask `tailscale-app` conflicts with that formula.
The formula has no `depends_on "tailscale"`.

Linuxbrew installs the x86_64 release zip from `cross-build.yml` (ubuntu-latest).
Linux arm64 has no zip in that workflow; use the Debian or RPM package there.
The Linux zip's glibc is newer than those packages.

Check the renderer without brew or a release:

```sh
packaging/homebrew/render-formula-test.sh
```
