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

impl StoreError {
    /// Allowlisted diagnostic category, never SQL detail, values, or source text.
    pub fn diagnostic_reason(&self) -> &'static str {
        match self {
            Self::Sqlx(error) => match error {
                sqlx_core::Error::PoolTimedOut => "pool_timeout",
                sqlx_core::Error::PoolClosed => "pool_closed",
                sqlx_core::Error::Io(_) => "database_io",
                sqlx_core::Error::Tls(_) => "database_tls",
                sqlx_core::Error::Database(error) => sqlstate_reason(error.code().as_deref()),
                _ => "database_client",
            },
            Self::Migrate(_) | Self::EmbeddedMigration(_) => "database_migration",
            Self::InvalidAccountSession(_) => "invalid_account_session",
            Self::OAuthSessionNotCurrent => "oauth_session_not_current",
        }
    }
}

fn sqlstate_reason(code: Option<&str>) -> &'static str {
    match code {
        Some("40P01") => "database_deadlock",
        Some("55P03") => "database_lock_timeout",
        Some("57014") => "database_query_canceled",
        Some(code) if code.starts_with("23") => "database_constraint",
        Some(code) if code.starts_with("08") => "database_connection",
        Some(code) if code.starts_with("40") => "database_transaction",
        _ => "database_query",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_are_allowlisted_and_distinguish_operational_failures() {
        for (code, expected) in [
            (Some("40P01"), "database_deadlock"),
            (Some("55P03"), "database_lock_timeout"),
            (Some("57014"), "database_query_canceled"),
            (Some("23505"), "database_constraint"),
            (Some("08006"), "database_connection"),
            (Some("40001"), "database_transaction"),
            (Some("untrusted detail"), "database_query"),
            (None, "database_query"),
        ] {
            assert_eq!(sqlstate_reason(code), expected);
        }
        assert_eq!(
            StoreError::Sqlx(sqlx_core::Error::PoolTimedOut).diagnostic_reason(),
            "pool_timeout"
        );
        assert_eq!(
            StoreError::Sqlx(sqlx_core::Error::PoolClosed).diagnostic_reason(),
            "pool_closed"
        );
        assert_eq!(
            StoreError::EmbeddedMigration("untrusted detail".into()).diagnostic_reason(),
            "database_migration"
        );
    }
}
