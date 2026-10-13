//! Error types for Axon.
//!
//! [`Error`] is the workspace-wide top-level error. Each crate defines its own
//! narrower `thiserror` enum and converts *into* `Error` (see e.g.
//! `axon_store::StoreError`). Because `axon-core` is the lowest crate and must
//! not depend on `axon-store`/`axon-api`, variants for those domains carry a
//! `String` message rather than the leaf error type directly — this keeps the
//! crate dependency graph acyclic. `anyhow` is used only at the `axon-server`
//! binary boundary, never in libraries.

use std::path::PathBuf;
use thiserror::Error;

/// Top-level error for the Axon workspace.
#[derive(Debug, Error)]
pub enum Error {
    /// Configuration failed to load or validate.
    #[error("configuration error: {0}")]
    Config(#[from] ConfigError),

    /// A storage-layer error (raised by `axon-store`). Carried as a string to
    /// avoid an `axon-core` → `axon-store` dependency cycle.
    #[error("store error: {0}")]
    Store(String),

    /// A sync-engine error (raised by `axon-sync`). Carried as a string for the
    /// same acyclicity reason as [`Error::Store`].
    #[error("sync error: {0}")]
    Sync(String),
}

/// Errors raised while loading [`crate::Config`].
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Figment failed to read a source or deserialize into the config struct.
    /// Boxed because `figment::Error` is large and would otherwise bloat every
    /// `Result` carrying it.
    #[error("failed to load configuration: {0}")]
    Figment(#[from] Box<figment::Error>),

    /// A caller requested a specific config file path, but it does not exist.
    #[error("configuration file does not exist: {0}")]
    MissingConfigFile(PathBuf),

    /// The system-wide config file is there, this user may not read it, and
    /// nothing else gives the process a database URL. Reported instead of "no
    /// configuration", which would offer first-time setup on a machine that is
    /// already set up.
    #[error(
        "{0} exists but this user cannot read it. Run the command as the service's \
         user (for a packaged install: `sudo -u axon axon-server …`), or pass --config"
    )]
    UnreadableSystemConfig(PathBuf),

    /// Configuration parsed but failed a semantic check (e.g. an account
    /// supplied neither or both of `password` / `access_token`). Validated
    /// outside figment so the message is human-readable.
    #[error("invalid configuration: {0}")]
    Validation(String),
}

/// Convenience alias for fallible APIs returning the top-level [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
