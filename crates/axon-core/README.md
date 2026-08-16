# axon-core

Shared types, errors, and configuration for Axon.

## Responsibility

Provides the `Config` loader (TOML file + environment overrides, via figment),
the top-level `Error` enum that downstream crate errors convert into, and any
primitive types shared across crates.

## Owns vs. consumes

- **Owns:** the typed configuration model and the workspace-wide `Error`/`Result`.
- **Consumed by:** every other Axon crate. Depends on no other `axon-*` crate.

## Public API surface

- `Config` — `server` (host/port), `database` (url/max_connections), `log` (level).
  - `Config::load(Option<&Path>)` — explicit file path + env overrides.
  - `Config::load_from(Option<&Path>)` — `--config`/explicit path, else discovery.
  - `Config::load_default()` — resolves `AXON_CONFIG`, else `./axon.toml`, else
    `<platform config dir>/axon.toml`, else env-only.
  - `Config::socket_addr()` — `SocketAddr` from `server.host`/`server.port`.
- `Error` (top-level), `ConfigError`, `Result<T>`.

## Notes

- Config precedence (lowest → highest): struct defaults → TOML file →
  `DATABASE_URL` → `AXON_`-prefixed env (`__` denotes nesting, e.g.
  `AXON_SERVER__PORT`).
- Downstream crates define their own `thiserror` enums and convert _into_
  `axon_core::Error`; because `axon-core` is the lowest crate, the `Store`
  variant carries a `String` rather than depending on `axon-store`.
