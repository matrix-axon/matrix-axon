//! Account-scoped SDK summary observations; never aggregate member state.
use sqlx_core::transaction::Transaction;
use sqlx_postgres::Postgres;
use uuid::Uuid;

use crate::{Store, StoreError};

/// A local SDK-cache observation, not an upstream synchronization timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomMemberCounts {
    pub joined: i64,
    pub invited: i64,
    pub observed_at: i64,
}

impl Store {
    /// Bound database execution as well as the caller's await. Dropping a
    /// timed-out future alone does not establish a PostgreSQL work deadline.
    pub(crate) async fn reconciliation_transaction(
        &self,
    ) -> Result<Transaction<'_, Postgres>, StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx_core::query::query("SELECT set_config('statement_timeout', '1500ms', true), set_config('lock_timeout', '1000ms', true)")
            .execute(&mut *tx).await?;
        Ok(tx)
    }

    /// Update only an existing summary. Purge/account deletion cannot be undone
    /// by a late watcher write. One account worker serializes observations.
    pub async fn set_room_member_counts(
        &self,
        account_id: Uuid,
        room_id: &str,
        counts: Option<RoomMemberCounts>,
    ) -> Result<(), StoreError> {
        let mut tx = self.reconciliation_transaction().await?;
        sqlx_core::query::query(
            "UPDATE room_summaries SET joined_member_count = $3, \
             invited_member_count = $4, member_counts_observed_at = $5 \
             WHERE account_id = $1 AND room_id = $2",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(counts.as_ref().map(|c| c.joined))
        .bind(counts.as_ref().map(|c| c.invited))
        .bind(counts.as_ref().map(|c| c.observed_at))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Primary-key read; historical/confirmed inaccessible rooms are unknown.
    pub async fn room_member_counts(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<Option<RoomMemberCounts>, StoreError> {
        let row: Option<(i64, i64, i64)> = sqlx_core::query_as::query_as::<Postgres, _>(
            "SELECT joined_member_count, invited_member_count, member_counts_observed_at \
             FROM room_summaries s WHERE account_id = $1 AND room_id = $2 \
             AND joined_member_count IS NOT NULL AND NOT hidden_left \
             AND NOT EXISTS (SELECT 1 FROM room_upstream_reconcile r \
                 WHERE r.account_id = s.account_id AND r.room_id = s.room_id AND r.state = 'gone')",
        )
        .bind(account_id)
        .bind(room_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(joined, invited, observed_at)| RoomMemberCounts {
            joined,
            invited,
            observed_at,
        }))
    }
}
