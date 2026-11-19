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

    /// Whether the database refused the statement because of the values it
    /// carried, so the same values fail on every attempt while other writes
    /// succeed: a data exception such as a `\u0000` escape in a `jsonb`
    /// (SQLSTATE class 22, here 22P05), a constraint violation (23), or a
    /// value past a size limit (54).
    ///
    /// Every other error is `false`, including deterministic ones that are not
    /// about the values (a protocol or TLS failure, a missing column). A caller
    /// that skips a write on `true` and retries it otherwise loses only the
    /// one row it could never store, while an error that would fail every
    /// write, such as a broken connection setup or a schema mismatch, holds
    /// its place instead of skipping everything it touches.
    pub fn is_value_rejection(&self) -> bool {
        matches!(
            self,
            Self::Sqlx(sqlx_core::Error::Database(error))
                if value_rejection_sqlstate(error.code().as_deref())
        )
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

/// The SQLSTATE classes [`StoreError::is_value_rejection`] covers: data
/// exceptions (22), integrity constraint violations (23), and program limits
/// exceeded, such as a row or index entry too large (54).
fn value_rejection_sqlstate(code: Option<&str>) -> bool {
    code.and_then(|code| code.get(..2))
        .is_some_and(|class| matches!(class, "22" | "23" | "54"))
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
    fn only_errors_about_the_values_are_value_rejections() {
        for code in [
            "22P05", "22021", "22001", "23505", "23503", "23514", "54000",
        ] {
            assert!(
                value_rejection_sqlstate(Some(code)),
                "{code} rejects the values"
            );
        }
        // Failures that would refuse any write: hold, don't skip.
        for code in [
            "08006", "40P01", "40001", "42703", "42P01", "53100", "55P03", "57014", "58030",
            "XX000", "P0001",
        ] {
            assert!(
                !value_rejection_sqlstate(Some(code)),
                "{code} is not about the values"
            );
        }
        assert!(!value_rejection_sqlstate(None));
        assert!(!value_rejection_sqlstate(Some("2")));
        for error in [
            sqlx_core::Error::PoolTimedOut,
            sqlx_core::Error::PoolClosed,
            sqlx_core::Error::Protocol("unexpected message".into()),
            sqlx_core::Error::RowNotFound,
        ] {
            assert!(!StoreError::Sqlx(error).is_value_rejection());
        }
        assert!(!StoreError::EmbeddedMigration("x".into()).is_value_rejection());
    }
}
