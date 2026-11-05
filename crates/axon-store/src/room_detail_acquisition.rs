//! Account-scoped cache of what acquisition learned from the homeserver
//! (ADR 0126 decisions 2-4): homeserver-local aliases, directory publication,
//! and confirmations that a metadata state type is absent.
//!
//! Never merged into `room_state`: synced state stays authoritative for what
//! it covers, and these rows are deleted on leave, purge, and account removal.
//! Every time here is Unix milliseconds from the PostgreSQL clock.
use serde_json::Value;
use uuid::Uuid;

use crate::{Store, StoreError};

/// What a row caches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DetailKind {
    /// `GET /rooms/{roomId}/aliases`.
    LocalAliases,
    /// `GET /directory/list/room/{roomId}`.
    DirectoryVisibility,
    /// `GET /rooms/{roomId}/state/{type}/`, one row per state type.
    Absence,
}

impl DetailKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DetailKind::LocalAliases => "local_aliases",
            DetailKind::DirectoryVisibility => "directory_visibility",
            DetailKind::Absence => "absence",
        }
    }
}

/// One cached row as a read sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct DetailRow {
    pub kind: String,
    /// The metadata state type of an absence row; empty otherwise.
    pub state_type: String,
    /// The last conclusive answer, or `None` when there has been none.
    pub value_status: Option<String>,
    pub value: Option<Value>,
    pub fetched_at: Option<i64>,
    pub expires_at: Option<i64>,
    /// The last failure's category, cleared by the next conclusive answer.
    pub failure: Option<String>,
    pub next_attempt_at: Option<i64>,
    /// Whether a read should schedule a refresh: there is no current value
    /// and no backoff in force.
    pub due: bool,
}

/// A room's cached rows and whether acquisition may reach the room at all.
#[derive(Debug, Clone, PartialEq)]
pub struct DetailSnapshot {
    /// The account is active and joined to the room. Only then may a read
    /// schedule a request; every other room stays unknown.
    pub eligible: bool,
    pub rows: Vec<DetailRow>,
}

/// How one acquisition request ended, as the cache records it.
#[derive(Debug, Clone, PartialEq)]
pub enum DetailOutcome {
    /// A conclusive answer. It replaces the value, including a denial after
    /// an earlier `available`, and expires after `ttl_ms`.
    Answer {
        status: &'static str,
        value: Option<Value>,
        ttl_ms: i64,
    },
    /// The state endpoint answered 200 for a type with no tuple: the state
    /// exists but sync has not delivered it. Clears any confirmation and
    /// waits `retry_in_ms` before asking again.
    Present { retry_in_ms: i64 },
    /// A transient failure. Keeps the value and backs off: one minute,
    /// doubling per consecutive failure up to `max_backoff_ms`, and never
    /// sooner than `retry_after_ms`.
    Failure {
        category: &'static str,
        retry_after_ms: Option<i64>,
        min_backoff_ms: i64,
        max_backoff_ms: i64,
    },
}

/// What recording an outcome did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailWrite {
    /// Something a client reads changed: the value, its freshness, or the
    /// failure category.
    Changed,
    /// Only timestamps advanced.
    Unchanged,
    /// The account left the room, was removed, or rejoined since the job
    /// started; or, for an absence confirmation, the state arrived meanwhile.
    /// Nothing was written.
    Fenced,
}

/// The room's membership epoch while the account is active and joined.
/// `$1` is the account, `$2` the room.
const JOINED_EPOCH: &str = "SELECT s.membership_epoch FROM room_summaries s \
    JOIN accounts ac ON ac.account_id = s.account_id \
    WHERE s.account_id = $1 AND s.room_id = $2 AND NOT s.hidden_left AND ac.state = 'active' \
      AND EXISTS (SELECT 1 FROM room_state m WHERE m.account_id = s.account_id \
          AND m.room_id = s.room_id AND m.event_type = 'm.room.member' \
          AND m.state_key = ac.user_id AND m.content->>'membership' = 'join' \
          AND m.redacted IS DISTINCT FROM true) \
      AND NOT EXISTS (SELECT 1 FROM room_upstream_reconcile r \
          WHERE r.account_id = s.account_id AND r.room_id = s.room_id AND r.state = 'gone')";

const NOW_MS: &str = "floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint";

/// The old row a write compares against.
struct Previous {
    value_status: Option<String>,
    value: Option<Value>,
    fetched_at: Option<i64>,
    expires_at: Option<i64>,
    failure: Option<String>,
    failures: i32,
}

impl Store {
    /// The cached rows of `kinds` for one room, with whether each is due, and
    /// whether the room is eligible for acquisition. A pure local read.
    pub async fn room_detail_snapshot(
        &self,
        account_id: Uuid,
        room_id: &str,
        kinds: &[DetailKind],
    ) -> Result<DetailSnapshot, StoreError> {
        let kinds: Vec<&str> = kinds.iter().map(|kind| kind.as_str()).collect();
        let sql = format!(
            "WITH clock AS (SELECT {NOW_MS} AS now) \
             SELECT kind, state_type, value_status, value, fetched_at, expires_at, failure, \
                    next_attempt_at, \
                    (expires_at IS NULL OR expires_at <= clock.now) \
                        AND (next_attempt_at IS NULL OR next_attempt_at <= clock.now) AS due \
             FROM room_detail_acquisitions, clock \
             WHERE account_id = $1 AND room_id = $2 AND kind = ANY($3) \
             ORDER BY kind, state_type"
        );
        type Row = (
            String,
            String,
            Option<String>,
            Option<Value>,
            Option<i64>,
            Option<i64>,
            Option<String>,
            Option<i64>,
            bool,
        );
        let rows: Vec<Row> = sqlx_core::query_as::query_as(&sql)
            .bind(account_id)
            .bind(room_id)
            .bind(&kinds)
            .fetch_all(&self.pool)
            .await?;
        let rows = rows
            .into_iter()
            .map(
                |(
                    kind,
                    state_type,
                    value_status,
                    value,
                    fetched_at,
                    expires_at,
                    failure,
                    next_attempt_at,
                    due,
                )| DetailRow {
                    kind,
                    state_type,
                    value_status,
                    value,
                    fetched_at,
                    expires_at,
                    failure,
                    next_attempt_at,
                    due,
                },
            )
            .collect();
        let eligible = self.joined_epoch(account_id, room_id).await?.is_some();
        Ok(DetailSnapshot { eligible, rows })
    }

    /// The membership epoch to fence a job with, or `None` when the account
    /// is not an active member of the room and no request may be made.
    pub async fn joined_epoch(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<Option<i64>, StoreError> {
        let epoch: Option<(i64,)> = sqlx_core::query_as::query_as(JOINED_EPOCH)
            .bind(account_id)
            .bind(room_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(epoch.map(|(epoch,)| epoch))
    }

    /// Of `types`, in that order, those with no state tuple whose absence
    /// confirmation is due.
    pub async fn due_absence_types(
        &self,
        account_id: Uuid,
        room_id: &str,
        types: &[&str],
    ) -> Result<Vec<String>, StoreError> {
        let sql = format!(
            "SELECT t.state_type FROM unnest($3::text[]) WITH ORDINALITY AS t(state_type, position) \
             WHERE NOT EXISTS (SELECT 1 FROM room_state rs WHERE rs.account_id = $1 \
                 AND rs.room_id = $2 AND rs.event_type = t.state_type AND rs.state_key = '') \
               AND NOT EXISTS (SELECT 1 FROM room_detail_acquisitions a \
                 WHERE a.account_id = $1 AND a.room_id = $2 AND a.kind = 'absence' \
                   AND a.state_type = t.state_type \
                   AND ((a.expires_at IS NOT NULL AND a.expires_at > {NOW_MS}) \
                        OR (a.next_attempt_at IS NOT NULL AND a.next_attempt_at > {NOW_MS}))) \
             ORDER BY t.position"
        );
        let rows: Vec<(String,)> = sqlx_core::query_as::query_as(&sql)
            .bind(account_id)
            .bind(room_id)
            .bind(types)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|(state_type,)| state_type).collect())
    }

    /// Record how a request ended, if the room's membership epoch is still
    /// `epoch` and the account is still an active member. An absence
    /// confirmation also requires that no tuple of its type exists, so state
    /// sync delivered during the request wins; it serializes with that
    /// tuple's insert on an advisory lock, so the two cannot both commit.
    /// Nothing is held across the request.
    ///
    /// Lock order: a share lock on the summary row, then (for a confirmation)
    /// the absence advisory lock, then the cache row. The only other holder of the advisory lock
    /// is the `room_state` insert trigger, which then deletes the cache row;
    /// no transaction that inserts `room_state` also writes `room_summaries`
    /// (`refresh_room_summary_display` and the epoch advance are statements
    /// of their own), and summary writers take the summary row first. With
    /// every path taking these locks in this order, no cycle can form.
    pub async fn record_room_detail(
        &self,
        account_id: Uuid,
        room_id: &str,
        epoch: i64,
        kind: DetailKind,
        state_type: &str,
        outcome: &DetailOutcome,
    ) -> Result<DetailWrite, StoreError> {
        debug_assert_eq!(kind == DetailKind::Absence, !state_type.is_empty());
        let mut tx = self.reconciliation_transaction().await?;
        let current: Option<(i64,)> =
            sqlx_core::query_as::query_as(&format!("{JOINED_EPOCH} FOR SHARE OF s"))
                .bind(account_id)
                .bind(room_id)
                .fetch_optional(&mut *tx)
                .await?;
        if current != Some((epoch,)) {
            return Ok(DetailWrite::Fenced);
        }
        let confirms_absence = matches!(
            outcome,
            DetailOutcome::Answer {
                status: "absent",
                ..
            }
        );
        if confirms_absence {
            // Taken before the check, in its own statement, so the check's
            // snapshot includes any tuple whose insert held the lock first,
            // and a tuple inserted after this commits sees the confirmation.
            sqlx_core::query::query("SELECT lock_room_absence($1, $2, $3)")
                .bind(account_id)
                .bind(room_id)
                .bind(state_type)
                .execute(&mut *tx)
                .await?;
            let (present,): (bool,) = sqlx_core::query_as::query_as(
                "SELECT EXISTS (SELECT 1 FROM room_state WHERE account_id = $1 \
                 AND room_id = $2 AND event_type = $3 AND state_key = '')",
            )
            .bind(account_id)
            .bind(room_id)
            .bind(state_type)
            .fetch_one(&mut *tx)
            .await?;
            if present {
                return Ok(DetailWrite::Fenced);
            }
        }
        let (now,): (i64,) = sqlx_core::query_as::query_as(&format!("SELECT {NOW_MS}"))
            .fetch_one(&mut *tx)
            .await?;
        type Old = (
            Option<String>,
            Option<Value>,
            Option<i64>,
            Option<i64>,
            Option<String>,
            i32,
        );
        let previous: Option<Old> = sqlx_core::query_as::query_as(
            "SELECT value_status, value, fetched_at, expires_at, failure, failures \
             FROM room_detail_acquisitions WHERE account_id = $1 AND room_id = $2 \
               AND kind = $3 AND state_type = $4 FOR UPDATE",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(kind.as_str())
        .bind(state_type)
        .fetch_optional(&mut *tx)
        .await?;
        let previous = previous.map(
            |(value_status, value, fetched_at, expires_at, failure, failures)| Previous {
                value_status,
                value,
                fetched_at,
                expires_at,
                failure,
                failures,
            },
        );
        let next = Next::from(previous.as_ref(), outcome, now);
        let changed = match &previous {
            None => true,
            Some(old) => {
                old.value_status != next.value_status
                    || old.value != next.value
                    || old.failure.as_deref() != next.failure
                    // A refresh of a stale value makes it fresh again.
                    || (next.expires_at.is_some_and(|at| at > now)
                        && old.expires_at.is_none_or(|at| at <= now))
            }
        };
        sqlx_core::query::query(
            "INSERT INTO room_detail_acquisitions (account_id, room_id, kind, state_type, \
                 value_status, value, fetched_at, expires_at, failure, failures, next_attempt_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
             ON CONFLICT (account_id, room_id, kind, state_type) DO UPDATE SET \
                 value_status = EXCLUDED.value_status, value = EXCLUDED.value, \
                 fetched_at = EXCLUDED.fetched_at, expires_at = EXCLUDED.expires_at, \
                 failure = EXCLUDED.failure, failures = EXCLUDED.failures, \
                 next_attempt_at = EXCLUDED.next_attempt_at",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(kind.as_str())
        .bind(state_type)
        .bind(next.value_status.as_deref())
        .bind(next.value.as_ref())
        .bind(next.fetched_at)
        .bind(next.expires_at)
        .bind(next.failure)
        .bind(next.failures)
        .bind(next.next_attempt_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(if changed {
            DetailWrite::Changed
        } else {
            DetailWrite::Unchanged
        })
    }
}

/// The row a write leaves behind.
struct Next {
    value_status: Option<String>,
    value: Option<Value>,
    fetched_at: Option<i64>,
    expires_at: Option<i64>,
    failure: Option<&'static str>,
    failures: i32,
    next_attempt_at: Option<i64>,
}

impl Next {
    fn from(previous: Option<&Previous>, outcome: &DetailOutcome, now: i64) -> Self {
        match outcome {
            DetailOutcome::Answer {
                status,
                value,
                ttl_ms,
            } => Next {
                value_status: Some((*status).to_owned()),
                value: value.clone(),
                fetched_at: Some(now),
                expires_at: Some(now.saturating_add(*ttl_ms)),
                failure: None,
                failures: 0,
                next_attempt_at: None,
            },
            DetailOutcome::Present { retry_in_ms } => Next {
                value_status: None,
                value: None,
                fetched_at: None,
                expires_at: None,
                failure: None,
                failures: 0,
                next_attempt_at: Some(now.saturating_add(*retry_in_ms)),
            },
            DetailOutcome::Failure {
                category,
                retry_after_ms,
                min_backoff_ms,
                max_backoff_ms,
            } => {
                let failures = previous.map_or(0, |old| old.failures).saturating_add(1);
                Next {
                    value_status: previous.and_then(|old| old.value_status.clone()),
                    value: previous.and_then(|old| old.value.clone()),
                    fetched_at: previous.and_then(|old| old.fetched_at),
                    expires_at: previous.and_then(|old| old.expires_at),
                    failure: Some(category),
                    failures,
                    next_attempt_at: Some(
                        now.saturating_add(
                            backoff_ms(failures, *min_backoff_ms, *max_backoff_ms)
                                .max(retry_after_ms.unwrap_or(0)),
                        ),
                    ),
                }
            }
        }
    }
}

/// `min` doubled per consecutive failure after the first, capped at `max`.
fn backoff_ms(failures: i32, min: i64, max: i64) -> i64 {
    let doublings = failures.saturating_sub(1).clamp(0, 30) as u32;
    min.saturating_mul(1_i64 << doublings).min(max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_the_minimum_and_caps() {
        let (min, max) = (60_000, 1_800_000);
        assert_eq!(backoff_ms(1, min, max), 60_000);
        assert_eq!(backoff_ms(2, min, max), 120_000);
        assert_eq!(backoff_ms(5, min, max), 960_000);
        assert_eq!(backoff_ms(6, min, max), 1_800_000);
        assert_eq!(backoff_ms(i32::MAX, min, max), 1_800_000);
    }
}
