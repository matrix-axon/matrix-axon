//! Storage-layer errors.

use thiserror::Error;

/// Errors raised by the storage layer.
#[derive(Debug, Error)]
pub enum StoreError {
    /// A connection or query failed.
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx_core::Error),

    /// A migration failed to apply.
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx_core::migrate::MigrateError),

    /// An embedded migration file had a malformed name (expected
    /// `<version>_<description>.sql` with an integer version) or non-UTF-8
    /// contents. This is a build-time mistake, surfaced at startup.
    #[error("invalid embedded migration: {0}")]
    EmbeddedMigration(String),

    /// An account's persisted authentication fields do not form one complete,
    /// current session. The message never contains token material.
    #[error("invalid account session: {0}")]
    InvalidAccountSession(String),

    /// A refresh write found that the account was removed or its authentication
    /// kind changed. This is a permanent lifecycle outcome rather than a
    /// transient database failure.
    #[error("OAuth session is no longer current")]
    OAuthSessionNotCurrent,
}

impl From<StoreError> for axon_core::Error {
    fn from(err: StoreError) -> Self {
        axon_core::Error::Store(err.to_string())
    }
}
