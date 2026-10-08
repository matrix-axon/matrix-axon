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

/// Explicit outcome: a missing or not-yet-joined projection needs bounded retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberCountWrite {
    Applied,
    Unchanged,
    Superseded,
    Retry,
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

    /// Write only a changed, newer observation to an existing joined summary.
    /// Database predicates enforce ordering across overlapping callers.
    pub async fn set_room_member_counts(
        &self,
        account_id: Uuid,
        room_id: &str,
        counts: RoomMemberCounts,
    ) -> Result<MemberCountWrite, StoreError> {
        self.write_member_counts(account_id, room_id, Some(&counts), counts.observed_at)
            .await
    }

    /// Positive non-joined evidence invalidates counts, retaining its watermark
    /// so a delayed pre-leave observation cannot win after a subsequent rejoin.
    pub async fn invalidate_room_member_counts(
        &self,
        account_id: Uuid,
        room_id: &str,
        observed_at: i64,
    ) -> Result<MemberCountWrite, StoreError> {
        self.write_member_counts(account_id, room_id, None, observed_at)
            .await
    }

    async fn write_member_counts(
        &self,
        account_id: Uuid,
        room_id: &str,
        counts: Option<&RoomMemberCounts>,
        observed_at: i64,
    ) -> Result<MemberCountWrite, StoreError> {
        let joined = counts.map(|c| c.joined);
        let invited = counts.map(|c| c.invited);
        let mut tx = self.reconciliation_transaction().await?;
        let (applied, unchanged, superseded): (bool, bool, bool) = sqlx_core::query_as::query_as::<Postgres, _>(
            "WITH target AS MATERIALIZED (
                SELECT hidden_left, joined_member_count, invited_member_count, member_counts_observed_at
                FROM room_summaries WHERE account_id = $1 AND room_id = $2
             ), updated AS (
                UPDATE room_summaries
                SET joined_member_count = $3, invited_member_count = $4, member_counts_observed_at = $5
                WHERE account_id = $1 AND room_id = $2
                  AND ($3::bigint IS NULL OR NOT hidden_left)
                  AND (member_counts_observed_at IS NULL OR member_counts_observed_at < $5
                      OR ($3::bigint IS NULL AND member_counts_observed_at = $5))
                  AND (joined_member_count, invited_member_count) IS DISTINCT FROM ($3::bigint, $4::bigint)
                RETURNING 1
             ) SELECT EXISTS (SELECT 1 FROM updated),
                EXISTS (SELECT 1 FROM target WHERE ($3::bigint IS NULL OR NOT hidden_left)
                    AND (joined_member_count, invited_member_count) IS NOT DISTINCT FROM ($3::bigint, $4::bigint)),
                EXISTS (SELECT 1 FROM target WHERE ($3::bigint IS NULL OR NOT hidden_left)
                    AND member_counts_observed_at >= $5)",
        )
        .bind(account_id).bind(room_id).bind(joined).bind(invited).bind(observed_at)
        .fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(if applied {
            MemberCountWrite::Applied
        } else if unchanged {
            MemberCountWrite::Unchanged
        } else if superseded {
            MemberCountWrite::Superseded
        } else {
            MemberCountWrite::Retry
        })
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
