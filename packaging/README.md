# Native Linux packages

nFPM metadata for `axon-server` `.deb` and `.rpm`.
Not a substitute for Docker Compose: Postgres is recommended locally, not bundled.

## Build

```sh
cargo build --release -p axon-server --bin axon-server
# https://github.com/goreleaser/nfpm/releases
packaging/package.sh
```

Artifacts land in `target/nfpm/`.
CI (`.github/workflows/package.yml`) does the same on `ubuntu-22.04` so Bookworm/Jammy can run the glibc binary.

## First install (local Postgres)

`apt install ./axon-server_*.deb` (or the rpm equivalent) with a reachable local cluster:

1. Creates Unix user `axon`.
2. Creates role `axon`, database `axon`, and `pgcrypto` as the `postgres` superuser.
3. Runs `axon-server init` as `axon` with a sqlx Unix-socket URL
   (`postgres://axon@%2Fvar%2Frun%2Fpostgresql/axon`, peer, no password).
4. Enables and starts `axon-server.service`.
5. Arms web bootstrap (`AXON_SERVER__BOOTSTRAP_WEB_AUTO`). URL is in `journalctl -u axon-server`.

Remote Postgres: the package still installs; see `README.Debian` in the doc directory of the built package.
