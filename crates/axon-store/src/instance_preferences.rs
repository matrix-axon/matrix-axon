//! Instance-wide preferences (ADRs 0103 and 0104).
//!
//! Not account-scoped: Axon is one human per process, while preferences can
//! span Matrix accounts (`space_order`) or clients (`message_gestures`) and
//! cannot live in Matrix account data or account-scoped `device_state`.
//! Last-write-wins on the whole JSON value; `updated_at` is maintained by
//! trigger.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};

use crate::{Store, StoreError};

/// One instance-preference row as read back from the store.
#[derive(Debug, Clone)]
pub struct InstancePreference {
    /// Allowlisted key, e.g. `space_order` or `message_gestures`.
    pub key: String,
    /// The stored JSON value.
    pub value: Value,
    /// Server-clock write time.
    pub updated_at: DateTime<Utc>,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for InstancePreference {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(InstancePreference {
            key: row.try_get("key")?,
            value: row.try_get("value")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

impl Store {
    /// Read one instance preference, or `None` if it has never been written.
    pub async fn instance_preference(
        &self,
        key: &str,
    ) -> Result<Option<InstancePreference>, StoreError> {
        let row = sqlx_core::query_as::query_as::<Postgres, InstancePreference>(
            "SELECT key, value, updated_at FROM instance_preferences WHERE key = $1",
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Last-write-wins upsert of one instance preference. Returns the
    /// statement-stable `updated_at` the row now carries.
    pub async fn upsert_instance_preference(
        &self,
        key: &str,
        value: &Value,
    ) -> Result<DateTime<Utc>, StoreError> {
        let row = sqlx_core::query::query(
            "INSERT INTO instance_preferences (key, value) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value \
             RETURNING updated_at",
        )
        .bind(key)
        .bind(value)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get("updated_at")?)
    }
}
