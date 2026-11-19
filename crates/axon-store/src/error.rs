//! Storage-layer errors.

use thiserror::Error;

/// Errors raised by the storage layer.
#[derive(Debug, Error)]
pub enum StoreError {
    /// Invalid database deadlines or pool sizing.
    #[error("{0}")]
    Config(#[from] axon_core::ConfigError),
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

    /// The database's `server_encoding` is not UTF8. Queries compare, trim and
    /// match Unicode text, which other encodings cannot represent (GH #803).
    #[error(
        "database encoding is {0}, but Axon requires UTF8; copy it with `pg_dump --encoding=UTF8` \
         into a database created with `createdb -E UTF8 -T template0 --locale=C`, and swap that \
         one in (docs/technical-reference.md, \"Database encoding\")"
    )]
    UnsupportedEncoding(String),

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
    /// Whether the database refused the write because a value that must be
    /// unique is already taken. A caller that generated the value can pick
    /// another and try again.
    pub fn is_unique_violation(&self) -> bool {
        matches!(self, Self::Sqlx(sqlx_core::Error::Database(error)) if error.is_unique_violation())
    }

    /// Whether the same statement can succeed if it is simply tried again: the
    /// database was unreachable, overloaded, shutting down, or lost a race
    /// (deadlock, serialization failure, lock or statement timeout). Anything
    /// else, such as a value the database rejects (a `\u0000` escape in a
    /// `jsonb`, SQLSTATE 22P05) or a constraint violation, fails the same way on
    /// every attempt.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Sqlx(error) => match error {
                sqlx_core::Error::PoolTimedOut
                | sqlx_core::Error::PoolClosed
                | sqlx_core::Error::Io(_)
                | sqlx_core::Error::Tls(_)
                | sqlx_core::Error::Protocol(_)
                | sqlx_core::Error::WorkerCrashed => true,
                sqlx_core::Error::Database(error) => transient_sqlstate(error.code().as_deref()),
                _ => false,
            },
            _ => false,
        }
    }

    /// Allowlisted diagnostic category, never SQL detail, values, or source text.
    pub fn diagnostic_reason(&self) -> &'static str {
        match self {
            Self::Config(_) => "database_configuration",
            Self::Sqlx(error) => match error {
                sqlx_core::Error::PoolTimedOut => "pool_timeout",
                sqlx_core::Error::PoolClosed => "pool_closed",
                sqlx_core::Error::Io(_) => "database_io",
                sqlx_core::Error::Tls(_) => "database_tls",
                sqlx_core::Error::Database(error) => sqlstate_reason(error.code().as_deref()),
                _ => "database_client",
            },
            Self::Migrate(_) | Self::EmbeddedMigration(_) => "database_migration",
            Self::UnsupportedEncoding(_) => "database_encoding",
            Self::InvalidAccountSession(_) => "invalid_account_session",
            Self::OAuthSessionNotCurrent => "oauth_session_not_current",
        }
    }
}

/// The SQLSTATEs [`StoreError::is_transient`] retries: connection exceptions
/// (08), transaction rollbacks such as deadlocks and serialization failures
/// (40), insufficient resources such as a full disk or too many connections
/// (53), operator intervention such as a statement timeout or a shutdown (57),
/// system errors such as an I/O failure (58), and a lock timeout (55P03).
fn transient_sqlstate(code: Option<&str>) -> bool {
    let Some(code) = code else {
        return false;
    };
    code == "55P03"
        || code
            .get(..2)
            .is_some_and(|class| matches!(class, "08" | "40" | "53" | "57" | "58"))
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

    #[test]
    fn only_errors_a_retry_can_clear_are_transient() {
        for code in [
            "08006", "40P01", "40001", "53100", "53300", "55P03", "57014", "57P01", "58030",
        ] {
            assert!(transient_sqlstate(Some(code)), "{code} is transient");
        }
        for code in [
            "22P05", "22021", "23505", "23503", "42703", "54000", "XX000", "P0001",
        ] {
            assert!(!transient_sqlstate(Some(code)), "{code} is permanent");
        }
        assert!(!transient_sqlstate(None));
        assert!(!transient_sqlstate(Some("5")));
        assert!(StoreError::Sqlx(sqlx_core::Error::PoolTimedOut).is_transient());
        assert!(StoreError::Sqlx(sqlx_core::Error::PoolClosed).is_transient());
        assert!(!StoreError::Sqlx(sqlx_core::Error::RowNotFound).is_transient());
        assert!(!StoreError::EmbeddedMigration("x".into()).is_transient());
    }
}
