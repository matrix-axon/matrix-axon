//! Rooms the homeserver appears to have stopped serving (ADR 0090).
//!
//! A room purged upstream — a deleted bridge portal, an admin room purge —
//! produces no signal Axon can observe: sync stops mentioning it, no leave or
//! tombstone arrives, and the local state store keeps reporting the account as
//! joined. Anything derived from the room then freezes, including the ADR 0070
//! unread count, which no read receipt can clear because the homeserver will
//! not accept one for a room it does not serve.
//!
//! These reads and writes are the durable half of a deliberately two-step
//! reconcile: a rejected room-scoped call records a cheap `suspect` row (no
//! network, no verdict), and a later bounded probe promotes it to `gone` or
//! clears it. Splitting the steps keeps the rejecting call's latency budget
//! intact and makes the flow crash-safe — a process killed between the two
//! resumes from the row on the next boot.
//!
//! Nothing here deletes room content; see the migration for why, and how a room
//! that comes back recovers.

use std::collections::HashSet;

use sqlx_postgres::Postgres;
use uuid::Uuid;

use crate::{Store, StoreError};

impl Store {
    /// Record that a room-scoped upstream call was definitively rejected for a
    /// room this account believes it is joined to.
    ///
    /// Insert-if-absent on purpose: a repeat rejection must not reset
    /// `first_flagged_at` (how long the room has looked wrong is the useful
    /// number), and must never demote an already-confirmed `gone` row back to
    /// `suspect`. `detail` is the rejecting error, kept for debugging a wrong
    /// verdict later.
    pub async fn flag_room_upstream_suspect(
        &self,
        account_id: Uuid,
        room_id: &str,
        detail: &str,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query(
            "INSERT INTO room_upstream_reconcile (account_id, room_id, state, detail) \
             VALUES ($1, $2, 'suspect', $3) \
             ON CONFLICT (account_id, room_id) DO NOTHING",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(detail)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Promote a `suspect` room to `gone`: a bounded probe has confirmed the
    /// homeserver does not serve it. Returns whether the promotion applied.
    ///
    /// Conditional on the row still being `suspect`, rather than an upsert,
    /// because a probe holds an upstream round trip open for many seconds and a
    /// genuine room-scoped call can succeed underneath it. That
    /// success clears the row — proving the room reachable with newer evidence
    /// than the probe holds — and an unconditional upsert would then write the
    /// stale verdict back over it. The room would be pinned to zero *and*
    /// invisible to future probes, which read only `suspect` rows, so nothing
    /// short of another successful call would ever undo it. Losing the race is
    /// the correct outcome: absence is the claim that needs proving, and the
    /// caller logs the discard.
    pub async fn mark_room_upstream_gone(
        &self,
        account_id: Uuid,
        room_id: &str,
        detail: &str,
    ) -> Result<bool, StoreError> {
        let result = sqlx_core::query::query(
            "UPDATE room_upstream_reconcile SET state = 'gone', detail = $3 \
             WHERE account_id = $1 AND room_id = $2 AND state = 'suspect'",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(detail)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Forget any reconcile state for a room — the probe found it healthy, or a
    /// room-scoped call succeeded, which is how a room that comes back (a
    /// restored purge, a re-created portal) recovers on its own. A no-op when no
    /// row exists, so callers may fire it unconditionally on success rather than
    /// reading first.
    pub async fn clear_room_upstream_reconcile(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query(
            "DELETE FROM room_upstream_reconcile WHERE account_id = $1 AND room_id = $2",
        )
        .bind(account_id)
        .bind(room_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Rooms awaiting a verification probe. Normally empty; served by a partial
    /// index on `state = 'suspect'` so the common case costs nothing.
    pub async fn suspect_upstream_rooms(
        &self,
        account_id: Uuid,
    ) -> Result<Vec<String>, StoreError> {
        let rows: Vec<(String,)> = sqlx_core::query_as::query_as::<Postgres, (String,)>(
            "SELECT room_id FROM room_upstream_reconcile \
             WHERE account_id = $1 AND state = 'suspect' ORDER BY first_flagged_at",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(room_id,)| room_id).collect())
    }

    /// Rooms confirmed absent upstream, for seeding the unread watcher's
    /// in-memory skip set at startup — the same seed-from-store pattern
    /// [`Store::room_unread_counts`] serves for the counts themselves.
    pub async fn rooms_gone_upstream(
        &self,
        account_id: Uuid,
    ) -> Result<HashSet<String>, StoreError> {
        let rows: Vec<(String,)> = sqlx_core::query_as::query_as::<Postgres, (String,)>(
            "SELECT room_id FROM room_upstream_reconcile WHERE account_id = $1 AND state = 'gone'",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(room_id,)| room_id).collect())
    }
}
