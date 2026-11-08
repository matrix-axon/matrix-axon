//! Per-device client state: drafts, read markers, and similar (M12).
//!
//! A generic KV projection scoped to `(account, device, namespace, key)`,
//! where a *device* is a client of axon (one TUI/web install) identified by a
//! client-supplied UUID — not a Matrix device. `axon-api` writes it from the
//! `PUT /v1/devices/{device_id}/state/{namespace}` handler and serves the
//! cross-device merged view from the matching GET. Values are opaque JSONB;
//! a `NULL` value is a tombstone so a deleted key wins the last-write-wins
//! merge instead of resurfacing another device's older value. See ADR 0048.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::{Store, StoreError};

/// One batch of device-state entries to upsert: the `(key, value)` pairs one
/// device writes to one namespace in a single atomic statement.
pub struct DeviceStateUpsert<'a> {
    /// Axon account this state belongs to.
    pub account_id: Uuid,
    /// The writing device (client-supplied UUID).
    pub device_id: Uuid,
    /// Client-chosen grouping, e.g. `drafts`, `read_markers`.
    pub namespace: &'a str,
    /// The entries to write. A `None` value writes a tombstone (the key was
    /// deleted). Must be non-empty.
    pub entries: &'a [(String, Option<Value>)],
}

/// One device-state row as read back from the store.
#[derive(Debug, Clone)]
pub struct DeviceStateRow {
    /// The device that wrote this row.
    pub device_id: Uuid,
    /// Key within the namespace.
    pub key: String,
    /// The opaque value; `None` is a tombstone.
    pub value: Option<Value>,
    /// Server-clock write time — the last-write-wins ordering.
    pub updated_at: DateTime<Utc>,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for DeviceStateRow {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        Ok(DeviceStateRow {
            device_id: row.try_get("device_id")?,
            key: row.try_get("key")?,
            value: row.try_get("value")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

/// The least a device-state delta reaches behind its cursor (ADR 0131).
const MIN_OVERLAP_SECS: i64 = 300;

/// How far a delta reaches behind its cursor: twice the ordinary pool's
/// statement timeout, with a floor of [`MIN_OVERLAP_SECS`].
///
/// A write's `updated_at` is its statement's start, not its commit, so a write
/// that started before a read and committed after it is older than that read's
/// cursor. Every device-state write is one autocommit statement bounded by the
/// statement timeout, so reaching back this far catches it.
pub(crate) fn overlap_for(statement_secs: u32) -> chrono::Duration {
    chrono::Duration::seconds((2 * i64::from(statement_secs)).max(MIN_OVERLAP_SECS))
}

/// One read of a namespace (ADR 0131): a full merged view or a delta, and the
/// cursor to pass back next time.
#[derive(Debug, Clone)]
pub struct DeviceStateRead {
    /// `true`: `rows` is the whole merged namespace, tombstones omitted.
    /// `false`: `rows` is every key written since the cursor, each with its
    /// current winner, tombstones included as `value: None`.
    pub full: bool,
    /// The rows, ordered by key.
    pub rows: Vec<DeviceStateRow>,
    /// The database clock taken just before this read; the next read's cursor.
    pub as_of: DateTime<Utc>,
}

impl Store {
    /// How far a delta reaches behind its cursor (ADR 0131).
    pub fn device_state_overlap(&self) -> chrono::Duration {
        self.device_state_overlap
    }

    /// Read one namespace, in full or as a delta since an earlier read's
    /// `as_of` (ADR 0131).
    ///
    /// The clock is read **before** the rows, so every write this read misses
    /// has an `updated_at` no older than `as_of − overlap` and is in the next
    /// delta. A `since` later than the database clock did not come from this
    /// clock, so it is answered in full rather than trusted.
    pub async fn read_device_state(
        &self,
        account_id: Uuid,
        namespace: &str,
        since: Option<DateTime<Utc>>,
    ) -> Result<DeviceStateRead, StoreError> {
        let as_of: DateTime<Utc> = sqlx_core::query_scalar::query_scalar("SELECT now()")
            .fetch_one(&self.pool)
            .await?;
        match since {
            Some(since) if since <= as_of => Ok(DeviceStateRead {
                full: false,
                rows: self
                    .device_state_changes(account_id, namespace, since - self.device_state_overlap)
                    .await?,
                as_of,
            }),
            _ => Ok(DeviceStateRead {
                full: true,
                rows: self.merged_device_state(account_id, namespace).await?,
                as_of,
            }),
        }
    }

    /// Every key of one namespace with a row written after `after`, each with
    /// its current winner across **all** the account's rows for that key —
    /// the same merge as [`Store::merged_device_state`] — and tombstone
    /// winners kept, so a client holding the key learns it was deleted.
    pub async fn device_state_changes(
        &self,
        account_id: Uuid,
        namespace: &str,
        after: DateTime<Utc>,
    ) -> Result<Vec<DeviceStateRow>, StoreError> {
        let rows = sqlx_core::query_as::query_as::<Postgres, DeviceStateRow>(
            "SELECT DISTINCT ON (key) device_id, key, value, updated_at \
               FROM device_state \
              WHERE account_id = $1 AND namespace = $2 \
                AND key IN ( \
                    SELECT key FROM device_state \
                     WHERE account_id = $1 AND namespace = $2 AND updated_at > $3 \
                ) \
              ORDER BY key ASC, updated_at DESC, device_id ASC",
        )
        .bind(account_id)
        .bind(namespace)
        .bind(after)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Upsert one device's batch of entries in a **single atomic statement**:
    /// either every entry commits or none does, so a failure can never leave a
    /// partially-applied merge, and one round-trip serves the whole PUT.
    /// Last-write-wins on the server clock: no freshness guard, the last write
    /// to arrive is authoritative (`updated_at` is bumped by trigger on
    /// update, `DEFAULT now()` on insert — and `now()` is statement-stable, so
    /// every row in the batch carries the **same** timestamp, which is what
    /// the caller reports on the wire).
    ///
    /// Tombstones ride the value array as JSON `null` and are folded to SQL
    /// `NULL` by `NULLIF` — unambiguous because the API layer already reserves
    /// JSON `null` to mean deletion, so it can never be a stored value.
    pub async fn upsert_device_state(
        &self,
        u: &DeviceStateUpsert<'_>,
    ) -> Result<DateTime<Utc>, StoreError> {
        let keys: Vec<&str> = u.entries.iter().map(|(key, _)| key.as_str()).collect();
        let values: Vec<Value> = u
            .entries
            .iter()
            .map(|(_, value)| value.clone().unwrap_or(Value::Null))
            .collect();
        let row = sqlx_core::query::query(
            "INSERT INTO device_state (account_id, device_id, namespace, key, value) \
             SELECT $1, $2, $3, entry.key, NULLIF(entry.value, 'null'::jsonb) \
               FROM UNNEST($4::text[], $5::jsonb[]) AS entry(key, value) \
             ON CONFLICT (account_id, device_id, namespace, key) DO UPDATE SET \
               value = EXCLUDED.value \
             RETURNING updated_at",
        )
        .bind(u.account_id)
        .bind(u.device_id)
        .bind(u.namespace)
        .bind(&keys)
        .bind(&values)
        .fetch_all(&self.pool)
        .await?;
        // Every returned row carries the same statement-stable timestamp; a
        // non-empty batch always returns rows.
        let row = row.first().ok_or(sqlx_core::Error::RowNotFound)?;
        Ok(row.try_get("updated_at")?)
    }

    /// The last-write-wins merged view of one namespace across *all* the
    /// account's devices: per key, the newest row wins (ties broken by
    /// `device_id` for determinism) and tombstone winners are dropped — a key
    /// whose freshest write was a delete is simply absent. This is what GET
    /// serves, so a client starting up sees the state its siblings left.
    pub async fn merged_device_state(
        &self,
        account_id: Uuid,
        namespace: &str,
    ) -> Result<Vec<DeviceStateRow>, StoreError> {
        let rows = sqlx_core::query_as::query_as::<Postgres, DeviceStateRow>(
            "SELECT device_id, key, value, updated_at FROM ( \
               SELECT DISTINCT ON (key) device_id, key, value, updated_at \
                 FROM device_state \
                WHERE account_id = $1 AND namespace = $2 \
                ORDER BY key ASC, updated_at DESC, device_id ASC \
             ) winners \
             WHERE value IS NOT NULL \
             ORDER BY key ASC",
        )
        .bind(account_id)
        .bind(namespace)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_is_twice_the_statement_timeout_with_a_floor() {
        assert_eq!(overlap_for(10), chrono::Duration::minutes(5));
        assert_eq!(overlap_for(150), chrono::Duration::minutes(5));
        assert_eq!(overlap_for(151), chrono::Duration::seconds(302));
        assert_eq!(overlap_for(86_400), chrono::Duration::days(2));
    }
}
