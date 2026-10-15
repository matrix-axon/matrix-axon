//! The SDK-calculated room display name on `room_summaries` (ADR 0118).
use sqlx_postgres::Postgres;
use uuid::Uuid;

use crate::{Store, StoreError};

/// Outcome of [`Store::set_room_display_name`]. `Missing` means the room has
/// no summary row yet (no stored event), so the caller should retry later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayNameWrite {
    Applied,
    Unchanged,
    Missing,
}

impl Store {
    /// Store `display_name` on the room's summary if it differs from the
    /// current value. One reconciler per account writes this column, one room
    /// at a time, so the compare-and-set only has to suppress no-op writes.
    pub async fn set_room_display_name(
        &self,
        account_id: Uuid,
        room_id: &str,
        display_name: &str,
    ) -> Result<DisplayNameWrite, StoreError> {
        let mut tx = self.reconciliation_transaction().await?;
        let (applied, exists): (bool, bool) = sqlx_core::query_as::query_as::<Postgres, _>(
            "WITH updated AS (
                UPDATE room_summaries SET display_name = $3
                WHERE account_id = $1 AND room_id = $2
                  AND display_name IS DISTINCT FROM $3
                RETURNING 1
             ) SELECT EXISTS (SELECT 1 FROM updated),
                EXISTS (SELECT 1 FROM room_summaries WHERE account_id = $1 AND room_id = $2)",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(display_name)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(if applied {
            DisplayNameWrite::Applied
        } else if exists {
            DisplayNameWrite::Unchanged
        } else {
            DisplayNameWrite::Missing
        })
    }
}
