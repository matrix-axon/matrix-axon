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
mod member_counts;
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
pub use member_counts::{MemberCountWrite, RoomMemberCounts};
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
use tokio::sync::{Mutex, Notify};

/// A handle to the Axon Postgres database.
///
/// Cheap to [`Clone`] (the underlying pool is reference-counted), so it can be
/// shared across axum handlers via router state.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
    // Hot reads have the configured concurrency, independently of auth/sync.
    read_pool: PgPool,
    status_pool: PgPool,
    index_pool: PgPool,
    // One connection, shared by all clones. Bulk work cannot take ordinary
    // connections away from authentication, sync, or request handlers.
    maintenance_pool: PgPool,
    refresh_sweep: Arc<Mutex<Option<Instant>>>,
    purge_retry_cursor: Arc<Mutex<Option<(uuid::Uuid, String)>>>,
    purge_wakeup: Arc<Notify>,
}

impl Store {
    /// Run migrations and verify an ordinary connection; other pools open lazily.
    /// Servers use `connect_with_config` to verify their full capacity at startup.
    pub async fn connect(database_url: &str, max_connections: u32) -> Result<Store, StoreError> {
        Self::connect_inner(
            &DatabaseConfig {
                url: database_url.to_owned(),
                max_connections,
                timeouts: DatabaseTimeouts::default(),
            },
            false,
        )
        .await
    }

    /// Open bounded ordinary, heavy-read, and maintenance pools from configuration.
    /// Migrations run first on a temporary pool with their own longer deadline.
    pub async fn connect_with_config(config: &DatabaseConfig) -> Result<Store, StoreError> {
        Self::connect_inner(config, true).await
    }

    /// Short-lived commands need one ordinary connection, not the server budget.
    /// Other pools stay lazy and keep the same deadlines if used.
    pub async fn connect_for_cli(config: &DatabaseConfig) -> Result<Store, StoreError> {
        Self::connect_inner(config, false).await
    }

    async fn connect_inner(config: &DatabaseConfig, verify_all: bool) -> Result<Store, StoreError> {
        config.validate()?;
        let migration_pool = Self::configured_pool(
            config,
            1,
            config.timeouts.migration_statement_secs,
            config.timeouts.migration_statement_secs,
            "axon-migrations",
        )?;
        tracing::info!("running database migrations");
        let result = migrations::embedded_migrator()?.run(&migration_pool).await;
        migration_pool.close().await;
        result?;

        let pool = Self::configured_pool(
            config,
            config.max_connections,
            config.timeouts.statement_secs,
            config.timeouts.lock_secs,
            "axon",
        )?;
        let read_pool = Self::configured_pool(
            config,
            config.max_connections,
            config.timeouts.statement_secs,
            config.timeouts.lock_secs,
            "axon-reads",
        )?;
        let status_pool = Self::configured_pool(
            config,
            1,
            config.timeouts.statement_secs,
            config.timeouts.lock_secs,
            "axon-status",
        )?;
        let maintenance_pool = Self::configured_pool(
            config,
            1,
            config.timeouts.maintenance_statement_secs,
            config.timeouts.lock_secs,
            "axon-maintenance",
        )?;
        let index_pool = Self::configured_pool(
            config,
            1,
            config.timeouts.maintenance_statement_secs,
            config.timeouts.lock_secs,
            "axon-index",
        )?;
        // Servers reserve the entire configured budget before reporting readiness.
        // CLI/test connections verify only the ordinary pool and leave others lazy.
        // Holding all server leases detects insufficient PostgreSQL slots at boot.
        let pools = [
            &pool,
            &read_pool,
            &status_pool,
            &maintenance_pool,
            &index_pool,
        ];
        let mut leases = Vec::new();
        for (index, candidate) in pools.iter().enumerate() {
            let required = if verify_all {
                candidate.options().get_max_connections()
            } else {
                u32::from(index == 0)
            };
            for _ in 0..required {
                match candidate.acquire().await {
                    Ok(connection) => leases.push(connection),
                    Err(error) => {
                        drop(leases);
                        for opened in pools {
                            opened.close().await;
                        }
                        return Err(error.into());
                    }
                }
            }
        }
        drop(leases);
        tracing::info!(
            max_connections = config.max_connections,
            full_capacity_verified = verify_all,
            statement_secs = config.timeouts.statement_secs,
            maintenance_statement_secs = config.timeouts.maintenance_statement_secs,
            migration_statement_secs = config.timeouts.migration_statement_secs,
            lock_secs = config.timeouts.lock_secs,
            acquire_secs = config.timeouts.acquire_secs,
            idle_transaction_secs = config.timeouts.idle_transaction_secs,
            "database pools verified; status and indexing isolated from hot reads and maintenance"
        );

        Ok(Store {
            pool,
            read_pool,
            status_pool,
            index_pool,
            maintenance_pool,
            refresh_sweep: Arc::new(Mutex::new(None)),
            purge_retry_cursor: Arc::new(Mutex::new(None)),
            purge_wakeup: Arc::new(Notify::new()),
        })
    }

    /// Developer repair uses the same longer lock and statement budget as migrations.
    pub fn migration_pool(config: &DatabaseConfig) -> Result<PgPool, StoreError> {
        config.validate()?;
        Self::configured_pool(
            config,
            1,
            config.timeouts.migration_statement_secs,
            config.timeouts.migration_statement_secs,
            "axon-db-repair",
        )
    }

    // Only called with validated configuration and internal pool sizes/deadlines.
    fn configured_pool(
        config: &DatabaseConfig,
        max_connections: u32,
        statement_secs: u32,
        lock_secs: u32,
        application_name: &str,
    ) -> Result<PgPool, StoreError> {
        let options = config
            .url
            .parse::<PgConnectOptions>()?
            .application_name(application_name);
        let idle_secs = config.timeouts.idle_transaction_secs;
        Ok(PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(u64::from(config.timeouts.acquire_secs)))
            .after_connect(move |connection, _| {
                Box::pin(async move {
                    // Session initialization avoids poolers' unsupported startup
                    // `options` parameter. Every replacement passes this hook before
                    // becoming available to callers; acquisition bounds initialization.
                    sqlx_core::query::query(
                        "SELECT set_config('statement_timeout', $1, false), \
                     set_config('lock_timeout', $2, false), \
                     set_config('idle_in_transaction_session_timeout', $3, false)",
                    )
                    .bind(format!("{statement_secs}s"))
                    .bind(format!("{lock_secs}s"))
                    .bind(format!("{idle_secs}s"))
                    .execute(connection)
                    .await?;
                    Ok(())
                })
            })
            .connect_lazy_with(options))
    }

    /// Access the underlying connection pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Isolated status connection; progress cannot queue behind hot message reads.
    pub fn status_pool(&self) -> &PgPool {
        &self.status_pool
    }

    /// Hot-read pool with the same configured concurrency as the ordinary pool.
    /// It uses ordinary statement deadlines and cannot consume auth/sync slots.
    pub fn read_pool(&self) -> &PgPool {
        &self.read_pool
    }
}
