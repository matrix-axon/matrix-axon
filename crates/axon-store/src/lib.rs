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
mod media_uploads;
mod migrations;
mod oauth_authorization_requests;
mod oauth_bind_requests;
mod oauth_identities;
mod oauth_refresh_tokens;
mod oauth_replay;
mod rooms;
mod search;
mod spaces;
mod state;
mod tokens;
mod unread;
mod upstream_reconcile;

pub use accounts::{Account, AccountState};
pub use backfill::{AccountBackfillProgress, RoomBackfillState};
pub use device_state::{DeviceStateRow, DeviceStateUpsert};
pub use error::StoreError;
pub use events::{
    EventCiphertext, EventCrypto, EventSenderTrust, NewEvent, PendingUtd, ReactionTally,
    ThreadSummary, TimelineCursor, TimelineRow,
};
pub use media_uploads::{MediaUpload, MediaUploadKind, MediaUploadState, NewMediaUpload};
pub use migrations::{embedded_migrations, EmbeddedMigration};
pub use oauth_authorization_requests::{AuthorizationRequest, NewAuthorizationRequest};
pub use oauth_bind_requests::BindRequest;
pub use oauth_identities::OauthIdentity;
pub use oauth_refresh_tokens::{RedeemRefreshTokenError, RotatedRefreshToken};
pub use rooms::RoomSummary;
pub use search::{
    room_purge_sentinel, IndexableEvent, SearchOutboxEntry, SEARCH_OUTBOX_PURGE,
    SEARCH_OUTBOX_ROOM_PURGE_PREFIX,
};
pub use spaces::{SpaceChildRow, SpaceParentRow};
pub use state::{AccountDataRow, AccountDataUpsert, RoomStateRow, RoomStateUpsert};
pub use tokens::{IssuedOAuthTokenPair, IssuedToken, Token};

use sqlx_postgres::{PgPool, PgPoolOptions};

/// A handle to the Axon Postgres database.
///
/// Cheap to [`Clone`] (the underlying pool is reference-counted), so it can be
/// shared across axum handlers via router state.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Connect a connection pool and run any pending migrations.
    pub async fn connect(database_url: &str, max_connections: u32) -> Result<Store, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(database_url)
            .await?;

        tracing::info!("running database migrations");
        migrations::embedded_migrator()?.run(&pool).await?;

        Ok(Store { pool })
    }

    /// Access the underlying connection pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}
