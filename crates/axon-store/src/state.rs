//! Room state and account data: the resolved, current-value projections.
//!
//! Where [`events`](crate::events) is an append-only log, these two tables hold
//! the *latest* value of each addressable piece of room state and account data,
//! upserted in place as syncs arrive. `axon-sync` writes them from the SDK's
//! state-event and account-data handlers; reads are point lookups (room name,
//! topic, members, fully-read markers, …). See ADR 0016.

use serde_json::Value;
use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::{Store, StoreError};

/// The `room_id` sentinel for global (account-wide) account data. A real Matrix
/// room id always starts with `!`, so the empty string is unambiguous. See the
/// `account_data` migration.
const GLOBAL_SCOPE: &str = "";

/// Singleton state types whose current value is cached on `room_summaries`
/// (ADR 0095). `state_key` must be `""`.
fn summary_display_state(event_type: &str, state_key: &str) -> bool {
    state_key.is_empty()
        && matches!(
            event_type,
            "m.room.name"
                | "m.room.topic"
                | "m.room.avatar"
                | "m.room.canonical_alias"
                | "m.room.create"
                | "m.room.tombstone"
        )
}

/// A single piece of current room state to upsert: the latest event holding the
/// `(room_id, event_type, state_key)` tuple for an account.
pub struct RoomStateUpsert<'a> {
    /// Axon account this state belongs to.
    pub account_id: Uuid,
    /// Matrix room ID.
    pub room_id: &'a str,
    /// State event type, e.g. `m.room.name`, `m.room.member`.
    pub event_type: &'a str,
    /// State key: `""` for singletons (`m.room.name`), the target user id for
    /// `m.room.member`, etc.
    pub state_key: &'a str,
    /// The event id currently holding this state tuple.
    pub event_id: &'a str,
    /// Matrix user ID of the sender that set this state.
    pub sender: &'a str,
    /// `origin_server_ts` in milliseconds — the freshness guard.
    pub origin_ts: i64,
    /// The state event `content`. `None` for a redacted state event.
    pub content: Option<Value>,
}

/// One resolved room-state row as read back from the store.
#[derive(Debug, Clone)]
pub struct RoomStateRow {
    /// Matrix room ID.
    pub room_id: String,
    /// State event type.
    pub event_type: String,
    /// State key (`""` for singletons).
    pub state_key: String,
    /// The event id currently holding this tuple.
    pub event_id: String,
    /// Sender that set this state.
    pub sender: String,
    /// `origin_server_ts` in milliseconds.
    pub origin_ts: i64,
    /// The state `content`, or `None` if redacted.
    pub content: Option<Value>,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for RoomStateRow {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(RoomStateRow {
            room_id: row.try_get("room_id")?,
            event_type: row.try_get("event_type")?,
            state_key: row.try_get("state_key")?,
            event_id: row.try_get("event_id")?,
            sender: row.try_get("sender")?,
            origin_ts: row.try_get("origin_ts")?,
            content: row.try_get("content")?,
        })
    }
}

/// Columns selected for a [`RoomStateRow`].
const ROOM_STATE_COLUMNS: &str =
    "room_id, event_type, state_key, event_id, sender, origin_ts, content";

/// A piece of account data to upsert. `room_id = None` is global (account-wide)
/// account data; `Some(room_id)` scopes it to a room.
pub struct AccountDataUpsert<'a> {
    /// Axon account this data belongs to.
    pub account_id: Uuid,
    /// `Some(room_id)` for per-room data, `None` for global.
    pub room_id: Option<&'a str>,
    /// Account-data event type, e.g. `m.push_rules`, `m.fully_read`, `m.tag`.
    pub event_type: &'a str,
    /// The account-data `content`.
    pub content: Value,
}

/// One account-data row as read back from the store.
#[derive(Debug, Clone)]
pub struct AccountDataRow {
    /// `Some(room_id)` for per-room data, `None` for global.
    pub room_id: Option<String>,
    /// Account-data event type.
    pub event_type: String,
    /// The account-data `content`.
    pub content: Value,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for AccountDataRow {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        let room_id: String = row.try_get("room_id")?;
        Ok(AccountDataRow {
            // Map the '' sentinel back to None at the API boundary.
            room_id: (room_id != GLOBAL_SCOPE).then_some(room_id),
            event_type: row.try_get("event_type")?,
            content: row.try_get("content")?,
        })
    }
}

impl Store {
    /// Upsert the current value of a room-state tuple. Idempotent in the
    /// re-delivery sense and **freshness-guarded**: an incoming event with an
    /// `origin_ts` older than the stored one is ignored, so a replay of historical
    /// state can never clobber newer state. Equal timestamps overwrite (harmless;
    /// they carry the same resolved value). `updated_at` is maintained by trigger.
    ///
    /// Callers that already know the account MXID should use
    /// [`Self::upsert_room_state_for_local_user`] so a busy room's other
    /// members do not each pay a `room_summaries` refresh (ADR 0095).
    pub async fn upsert_room_state(&self, s: &RoomStateUpsert<'_>) -> Result<(), StoreError> {
        self.upsert_room_state_for_local_user(s, None).await
    }

    /// [`Self::upsert_room_state`] with the account's MXID, when the caller
    /// already has it (the sync engine's `PersistContext::local_user_id`).
    ///
    /// `m.room.member` refreshes `hidden_left` only when `state_key` equals
    /// that id. Passing `None` never refreshes on member events — tests that
    /// drive leave/ban through this type pass `Some(account_user_id)`.
    pub async fn upsert_room_state_for_local_user(
        &self,
        s: &RoomStateUpsert<'_>,
        local_user_id: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query(
            "INSERT INTO room_state \
             (account_id, room_id, event_type, state_key, event_id, sender, origin_ts, content) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (account_id, room_id, event_type, state_key) DO UPDATE SET \
               event_id = EXCLUDED.event_id, \
               sender = EXCLUDED.sender, \
               origin_ts = EXCLUDED.origin_ts, \
               content = EXCLUDED.content \
             WHERE EXCLUDED.origin_ts >= room_state.origin_ts",
        )
        .bind(s.account_id)
        .bind(s.room_id)
        .bind(s.event_type)
        .bind(s.state_key)
        .bind(s.event_id)
        .bind(s.sender)
        .bind(s.origin_ts)
        .bind(&s.content)
        .execute(&self.pool)
        .await?;
        if summary_display_state(s.event_type, s.state_key)
            || (s.event_type == "m.room.member" && local_user_id == Some(s.state_key))
        {
            self.refresh_room_summary_display(s.account_id, s.room_id)
                .await?;
        }
        Ok(())
    }

    /// Read a single resolved room-state tuple, or `None` if unset.
    pub async fn room_state(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_type: &str,
        state_key: &str,
    ) -> Result<Option<RoomStateRow>, StoreError> {
        let sql = format!(
            "SELECT {ROOM_STATE_COLUMNS} FROM room_state \
             WHERE account_id = $1 AND room_id = $2 AND event_type = $3 AND state_key = $4"
        );
        let row = sqlx_core::query_as::query_as::<Postgres, RoomStateRow>(&sql)
            .bind(account_id)
            .bind(room_id)
            .bind(event_type)
            .bind(state_key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// Read every resolved state tuple of one type in a room, ordered by
    /// `state_key` — e.g. all `m.room.member` rows for a membership list.
    pub async fn room_state_of_type(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_type: &str,
    ) -> Result<Vec<RoomStateRow>, StoreError> {
        let sql = format!(
            "SELECT {ROOM_STATE_COLUMNS} FROM room_state \
             WHERE account_id = $1 AND room_id = $2 AND event_type = $3 \
             ORDER BY state_key ASC"
        );
        let rows = sqlx_core::query_as::query_as::<Postgres, RoomStateRow>(&sql)
            .bind(account_id)
            .bind(room_id)
            .bind(event_type)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// Upsert a piece of account data, global or per-room. Last-write-wins:
    /// account-data events carry no timestamp, so unlike room state there is no
    /// freshness guard — the newest sync is authoritative. `updated_at` is
    /// maintained by trigger.
    pub async fn upsert_account_data(&self, d: &AccountDataUpsert<'_>) -> Result<(), StoreError> {
        sqlx_core::query::query(
            "INSERT INTO account_data (account_id, room_id, event_type, content) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (account_id, room_id, event_type) DO UPDATE SET \
               content = EXCLUDED.content",
        )
        .bind(d.account_id)
        // Map None -> '' so global rows share one PK slot per type.
        .bind(d.room_id.unwrap_or(GLOBAL_SCOPE))
        .bind(d.event_type)
        .bind(&d.content)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Read one account-data value. `room_id = None` reads global data;
    /// `Some(room_id)` reads that room's data. `None` if unset.
    pub async fn account_data(
        &self,
        account_id: Uuid,
        room_id: Option<&str>,
        event_type: &str,
    ) -> Result<Option<AccountDataRow>, StoreError> {
        let row = sqlx_core::query_as::query_as::<Postgres, AccountDataRow>(
            "SELECT room_id, event_type, content FROM account_data \
             WHERE account_id = $1 AND room_id = $2 AND event_type = $3",
        )
        .bind(account_id)
        .bind(room_id.unwrap_or(GLOBAL_SCOPE))
        .bind(event_type)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }
}
