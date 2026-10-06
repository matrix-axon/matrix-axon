//! Postgres-backed event store, room state, and account data.
//!
//! `axon-store` owns all Postgres connections. Other crates (notably
//! `axon-api`) consume a cheaply-cloneable [`Store`] handle rather than talking
//! to the database directly. Migrations live under `migrations/` and are
//! embedded into the binary at compile time, so a deployed `axon` needs no
//! migration files on disk.

mod accounts;
mod backfill;
mod device_state;
mod error;
mod events;
mod instance_preferences;
mod invites;
mod matrix_oauth_acquire;
mod media_uploads;
mod migrations;
mod oauth_authorization_requests;
mod oauth_bind_requests;
mod oauth_identities;
mod oauth_native;
mod oauth_refresh_tokens;
mod rooms;
mod search;
mod spaces;
mod state;
mod tokens;
mod unread;
mod upstream_reconcile;

pub use accounts::{
    Account, AccountAuthKind, AccountState, MatrixOAuthRegistration, StoredAccountSession,
};
pub use backfill::{AccountBackfillProgress, RoomBackfillState};
pub use device_state::{DeviceStateRow, DeviceStateUpsert};
pub use error::StoreError;
pub use events::{
    EventCiphertext, EventCrypto, EventSenderTrust, NewEvent, PendingUtd, ReactionTally,
    ThreadSummary, TimelineCursor, TimelineRow,
};
pub use instance_preferences::InstancePreference;
pub use invites::{RoomInvite, RoomInviteSnapshot};
pub use matrix_oauth_acquire::{
    CommitMatrixOAuthAcquire, MatrixOAuthAcquireBreadcrumb, MatrixOAuthAcquireFinalization,
};
pub use media_uploads::{MediaUpload, MediaUploadKind, MediaUploadState, NewMediaUpload};
pub use migrations::{embedded_migrations, EmbeddedMigration};
pub use oauth_authorization_requests::{AuthorizationRequest, NewAuthorizationRequest};
pub use oauth_bind_requests::BindRequest;
pub use oauth_identities::{IdentityRemoval, OauthIdentity};
pub use oauth_native::{
    IdentityRedemption, IdentityRedemptionRejection, NativeChallenge, NATIVE_CHALLENGE_TTL_SECS,
};
pub use oauth_refresh_tokens::{RedeemRefreshTokenError, RotatedRefreshToken};
pub use rooms::{RoomSummary, RoomTag};
pub use search::{
    room_purge_sentinel, IndexableEvent, SearchOutboxEntry, SEARCH_OUTBOX_PURGE,
    SEARCH_OUTBOX_ROOM_PURGE_PREFIX,
};
pub use spaces::{SpaceChildRow, SpaceParentRow};
pub use state::{
    AccountDataRow, AccountDataUpsert, RoomMetadataStateRow, RoomStateRedaction, RoomStateRow,
    RoomStateUpsert,
};
pub use tokens::{IssuedOAuthTokenPair, IssuedToken, Token, TokenRevocation, VerifiedToken};

use std::sync::Arc;
use std::time::{Duration, Instant};

use axon_core::{DatabaseConfig, DatabaseTimeouts};
use sqlx_postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use tokio::sync::Mutex;

/// A handle to the Axon Postgres database.
///
/// Cheap to [`Clone`] (the underlying pool is reference-counted), so it can be
/// shared across axum handlers via router state.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
    // Expensive projections share one connection, independently of auth/sync.
    read_pool: PgPool,
    // One connection, shared by all clones. Bulk work cannot take ordinary
    // connections away from authentication, sync, or request handlers.
    maintenance_pool: PgPool,
    refresh_sweep: Arc<Mutex<Option<Instant>>>,
}

impl Store {
    /// Connect a connection pool and run any pending migrations.
    pub async fn connect(database_url: &str, max_connections: u32) -> Result<Store, StoreError> {
        Self::connect_with_config(&DatabaseConfig {
            url: database_url.to_owned(),
            max_connections,
            timeouts: DatabaseTimeouts::default(),
        })
        .await
    }

    /// Open bounded ordinary, heavy-read, and maintenance pools from configuration.
    /// Migrations run first on a temporary pool with their own longer deadline.
    pub async fn connect_with_config(config: &DatabaseConfig) -> Result<Store, StoreError> {
        config.validate()?;
        let migration_pool = Self::database_pool(
            config,
            1,
            config.timeouts.migration_statement_secs,
            "axon-migrations",
        )?;
        tracing::info!("running database migrations");
        let result = migrations::embedded_migrator()?.run(&migration_pool).await;
        migration_pool.close().await;
        result?;

        let pool = Self::database_pool(
            config,
            config.max_connections,
            config.timeouts.statement_secs,
            "axon",
        )?;
        // Verify connectivity before reporting a ready store.
        let connection = pool.acquire().await?;
        drop(connection);
        let maintenance_pool = Self::database_pool(
            config,
            1,
            config.timeouts.maintenance_statement_secs,
            "axon-maintenance",
        )?;
        let read_pool =
            Self::database_pool(config, 1, config.timeouts.statement_secs, "axon-reads")?;
        tracing::info!(
            max_connections = config.max_connections,
            statement_secs = config.timeouts.statement_secs,
            maintenance_statement_secs = config.timeouts.maintenance_statement_secs,
            migration_statement_secs = config.timeouts.migration_statement_secs,
            lock_secs = config.timeouts.lock_secs,
            acquire_secs = config.timeouts.acquire_secs,
            idle_transaction_secs = config.timeouts.idle_transaction_secs,
            "database deadlines configured; heavy reads and bulk maintenance each have one separate connection"
        );

        Ok(Store {
            pool,
            read_pool,
            maintenance_pool,
            refresh_sweep: Arc::new(Mutex::new(None)),
        })
    }

    /// Configure limits in the startup packet so every connection, including
    /// replacements after cancellation, has server-side deadlines from birth.
    /// Also used by developer repair, which deliberately bypasses migrations.
    pub fn database_pool(
        config: &DatabaseConfig,
        max_connections: u32,
        statement_secs: u32,
        application_name: &str,
    ) -> Result<PgPool, StoreError> {
        config.validate()?;
        if max_connections == 0 || !(1..=86_400).contains(&statement_secs) {
            return Err(axon_core::ConfigError::Validation(
                "database pool size and statement deadline must be positive and bounded".into(),
            )
            .into());
        }
        let options = config
            .url
            .parse::<PgConnectOptions>()?
            .application_name(application_name)
            .options([
                ("statement_timeout", format!("{statement_secs}s")),
                ("lock_timeout", format!("{}s", config.timeouts.lock_secs)),
                (
                    "idle_in_transaction_session_timeout",
                    format!("{}s", config.timeouts.idle_transaction_secs),
                ),
            ]);
        Ok(PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(u64::from(config.timeouts.acquire_secs)))
            .connect_lazy_with(options))
    }

    /// Access the underlying connection pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The separate single-connection pool for expensive read projections.
    /// It uses ordinary statement deadlines and cannot consume auth/sync slots.
    pub fn read_pool(&self) -> &PgPool {
        &self.read_pool
    }
}
