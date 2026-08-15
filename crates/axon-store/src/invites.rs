//! SDK-derived pending invites (issue #279, ADR 0091).
//!
//! Axon does not invent invite state — matrix-sdk's invited `Room` is the
//! source. A watcher in `axon-sync` caches the display snapshot here so a
//! fresh client load can list pending invites without the room ever having
//! landed in `events`.

use chrono::{DateTime, Utc};
use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::{Store, StoreError};

/// Display snapshot for one pending invite. Equality is the watcher's dedup
/// key: identical snapshots neither rewrite the row nor re-broadcast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomInviteSnapshot {
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub topic: Option<String>,
    pub canonical_alias: Option<String>,
    pub room_type: Option<String>,
    pub inviter_user_id: String,
    pub inviter_display_name: Option<String>,
    pub is_direct: bool,
    pub encrypted: bool,
}

/// One invite as it appears in the cross-account inbox.
#[derive(Debug, Clone)]
pub struct RoomInvite {
    pub account_id: Uuid,
    pub account_user_id: String,
    pub room_id: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub topic: Option<String>,
    pub canonical_alias: Option<String>,
    pub room_type: Option<String>,
    pub inviter_user_id: String,
    pub inviter_display_name: Option<String>,
    pub is_direct: bool,
    pub encrypted: bool,
    pub invited_at: DateTime<Utc>,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for RoomInvite {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(RoomInvite {
            account_id: row.try_get("account_id")?,
            account_user_id: row.try_get("account_user_id")?,
            room_id: row.try_get("room_id")?,
            name: row.try_get("name")?,
            avatar_url: row.try_get("avatar_url")?,
            topic: row.try_get("topic")?,
            canonical_alias: row.try_get("canonical_alias")?,
            room_type: row.try_get("room_type")?,
            inviter_user_id: row.try_get("inviter_user_id")?,
            inviter_display_name: row.try_get("inviter_display_name")?,
            is_direct: row.try_get("is_direct")?,
            encrypted: row.try_get("encrypted")?,
            invited_at: row.try_get("invited_at")?,
        })
    }
}

/// Internal `FromRow` so `room_invite_snapshots` does not bind a 10-tuple
/// (clippy `type_complexity`).
struct InviteSnapshotRow {
    room_id: String,
    snapshot: RoomInviteSnapshot,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for InviteSnapshotRow {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(Self {
            room_id: row.try_get("room_id")?,
            snapshot: RoomInviteSnapshot {
                name: row.try_get("name")?,
                avatar_url: row.try_get("avatar_url")?,
                topic: row.try_get("topic")?,
                canonical_alias: row.try_get("canonical_alias")?,
                room_type: row.try_get("room_type")?,
                inviter_user_id: row.try_get("inviter_user_id")?,
                inviter_display_name: row.try_get("inviter_display_name")?,
                is_direct: row.try_get("is_direct")?,
                encrypted: row.try_get("encrypted")?,
            },
        })
    }
}

impl Store {
    /// List pending invites across accounts, newest first. `account_id =
    /// Some(id)` narrows to one account; `None` returns every active
    /// account's invites. Deactivated / deleting accounts are excluded,
    /// matching [`Store::list_rooms`].
    pub async fn list_invites(
        &self,
        account_id: Option<Uuid>,
    ) -> Result<Vec<RoomInvite>, StoreError> {
        let rows = sqlx_core::query_as::query_as::<Postgres, RoomInvite>(
            "SELECT i.account_id, ac.user_id AS account_user_id, i.room_id, \
                    i.name, i.avatar_url, i.topic, i.canonical_alias, i.room_type, \
                    i.inviter_user_id, i.inviter_display_name, i.is_direct, \
                    i.encrypted, i.invited_at \
             FROM room_invites i \
             JOIN accounts ac ON ac.account_id = i.account_id AND ac.state = 'active' \
             WHERE ($1::uuid IS NULL OR i.account_id = $1) \
             ORDER BY i.invited_at DESC, i.room_id",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Upsert one invite's display snapshot. `invited_at` is set only on
    /// insert; a later refresh of name/avatar/inviter does not bump it.
    /// Returns the (possibly pre-existing) `invited_at`.
    pub async fn upsert_room_invite(
        &self,
        account_id: Uuid,
        room_id: &str,
        snapshot: &RoomInviteSnapshot,
    ) -> Result<DateTime<Utc>, StoreError> {
        let invited_at = sqlx_core::query_scalar::query_scalar::<Postgres, DateTime<Utc>>(
            "INSERT INTO room_invites ( \
                 account_id, room_id, name, avatar_url, topic, canonical_alias, \
                 room_type, inviter_user_id, inviter_display_name, is_direct, \
                 encrypted \
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
             ON CONFLICT (account_id, room_id) DO UPDATE SET \
               name = EXCLUDED.name, \
               avatar_url = EXCLUDED.avatar_url, \
               topic = EXCLUDED.topic, \
               canonical_alias = EXCLUDED.canonical_alias, \
               room_type = EXCLUDED.room_type, \
               inviter_user_id = EXCLUDED.inviter_user_id, \
               inviter_display_name = EXCLUDED.inviter_display_name, \
               is_direct = EXCLUDED.is_direct, \
               encrypted = EXCLUDED.encrypted \
             RETURNING invited_at",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(&snapshot.name)
        .bind(&snapshot.avatar_url)
        .bind(&snapshot.topic)
        .bind(&snapshot.canonical_alias)
        .bind(&snapshot.room_type)
        .bind(&snapshot.inviter_user_id)
        .bind(&snapshot.inviter_display_name)
        .bind(snapshot.is_direct)
        .bind(snapshot.encrypted)
        .fetch_one(&self.pool)
        .await?;
        Ok(invited_at)
    }

    /// This account's currently-persisted invite snapshots, keyed by room
    /// id. Seeds the `axon-sync` watcher's in-memory dedup cache so a
    /// restart does not treat every standing invite as new.
    pub async fn room_invite_snapshots(
        &self,
        account_id: Uuid,
    ) -> Result<Vec<(String, RoomInviteSnapshot)>, StoreError> {
        let rows = sqlx_core::query_as::query_as::<Postgres, InviteSnapshotRow>(
            "SELECT room_id, name, avatar_url, topic, canonical_alias, room_type, \
                    inviter_user_id, inviter_display_name, is_direct, encrypted \
             FROM room_invites WHERE account_id = $1",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.room_id, row.snapshot))
            .collect())
    }

    /// Drop one invite row. Returns whether a row was deleted.
    pub async fn delete_room_invite(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<bool, StoreError> {
        let result = sqlx_core::query::query(
            "DELETE FROM room_invites WHERE account_id = $1 AND room_id = $2",
        )
        .bind(account_id)
        .bind(room_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

// There is deliberately no "delete every row not in this list" primitive.
// The only list Axon could pass is the SDK's `invited_rooms()`, which is a
// view of partially-hydrated in-memory state, so set-difference against it
// deletes invites that are merely not loaded yet. Invites are dropped one
// room at a time, on positive evidence, via `delete_room_invite` (ADR 0091).
