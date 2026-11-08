//! Room summaries of one selected unjoined room (ADR 0128 decision 5).
//!
//! A summary answer is a `room_discovery` row with source `summary`, shared
//! with hierarchy crawls; `room_discovery_refresh` keeps the endpoint's
//! support, the last failure, and the backoff. A room qualifies only while
//! the account has not joined it and a joined space links it with a current
//! `m.space.child`, so neither the read nor the worker can probe arbitrary
//! rooms (decision 2).
//!
//! Lock order matches the crawl's: share locks on the linking spaces'
//! `room_summaries` rows in room id order, then the room's `room_discovery`
//! and refresh rows. A leave of the last linking space updates its summary
//! row before its trigger deletes discovery rows, so it waits for a summary
//! commit in flight and then removes what that commit wrote.
use serde_json::Value;
use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::room_detail_acquisition::{backoff_ms, NOW_MS};
use crate::space_discovery::{discovered_json, joined_child, DiscoveredEntry, DiscoveredRow};
use crate::spaces::SPACE_LINK_IS_CURRENT;
use crate::{Store, StoreError};

/// The limits a summary write applies (ADR 0128 decision 10), passed in by
/// the worker that owns them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryLimits {
    /// How long a `200` answer stays fresh.
    pub summary_ttl_ms: i64,
    /// ADR 0126's failure backoff.
    pub min_backoff_ms: i64,
    pub max_backoff_ms: i64,
}

/// A room's summary refresh state as a read sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryRefreshRow {
    /// `unknown`, `supported`, or `unsupported`.
    pub support: String,
    pub failure: Option<String>,
    /// The end of a failure backoff, or when an `unsupported` endpoint is
    /// checked again.
    pub next_attempt_at: Option<i64>,
}

/// A room's discovery row and summary refresh, and whether a read may ask
/// for a summary.
#[derive(Debug, Clone, PartialEq)]
pub struct RoomSummarySnapshot {
    /// The account is active, has not joined the room, and a joined space
    /// links it with a current `m.space.child`.
    pub eligible: bool,
    /// Eligible, the summary is unknown or expired, and no backoff or
    /// `unsupported` answer is in force.
    pub due: bool,
    /// The account's own membership in the room is `join`.
    pub joined: bool,
    /// Eligible, and the summary endpoint is `unsupported` until a later
    /// time: the room's summary can come only from a crawl of a linking
    /// space (decision 5's fallback).
    pub unsupported: bool,
    pub discovered: Option<DiscoveredRow>,
    pub refresh: Option<SummaryRefreshRow>,
}

/// Where a summary job starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryBegin {
    /// Request a summary. `vias` are the distinct `via` servers of every
    /// current link from a joined space, in space id order and then link
    /// order; the worker validates and bounds them.
    Request { vias: Vec<String> },
    /// The summary is fresh, or a backoff or `unsupported` answer is in
    /// force.
    NotDue,
    /// The room is joined, unknown, or not linked from a joined space.
    Ineligible,
}

/// How one summary request ended, as the cache records it.
#[derive(Debug, Clone, PartialEq)]
pub enum SummaryStep {
    /// `200`: the room's validated summary.
    Answer(DiscoveredEntry),
    /// `403` or `404 M_NOT_FOUND`: `forbidden` or `not_found`, with no fields.
    Denied { status: &'static str, ttl_ms: i64 },
    /// The homeserver lacks the endpoint, by `/versions` or by its answer.
    Unsupported { ttl_ms: i64 },
    /// A transient failure: ADR 0126's backoff, and never sooner than
    /// `retry_after_ms`. The last answer stays.
    Failure {
        category: &'static str,
        retry_after_ms: Option<i64>,
    },
}

/// What recording a summary step did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SummaryWrite {
    /// The room was joined, or no joined space links it any more; nothing
    /// was written.
    pub fenced: bool,
    /// The discovery row, the endpoint's support, or the failure category a
    /// read shows changed.
    pub changed: bool,
    /// Consecutive transient failures after the write.
    pub failures: i32,
}

/// SQL selecting `(space_id, via)` for each joined space of account `$1` that
/// links room `$2` with a current `m.space.child`, in space id order.
/// "Joined" is ADR 0126's acquisition rule (`JOINED_EPOCH`): an active
/// account, a summary row that is not `hidden_left`, an own `join` member
/// event, and no `gone` reconcile. `suffix` follows the `ORDER BY`, such as
/// a row lock.
fn linking_spaces(suffix: &str) -> String {
    format!(
        "SELECT l.room_id AS space_id, l.content->'via' AS via \
         FROM (SELECT room_id, content FROM room_state \
               WHERE account_id = $1 AND event_type = 'm.space.child' AND state_key = $2 \
                 AND room_id <> $2 AND {SPACE_LINK_IS_CURRENT}) l \
         JOIN room_summaries s ON s.account_id = $1 AND s.room_id = l.room_id \
         JOIN accounts ac ON ac.account_id = $1 \
         WHERE NOT s.hidden_left AND ac.state = 'active' \
           AND EXISTS (SELECT 1 FROM room_state m WHERE m.account_id = $1 \
               AND m.room_id = s.room_id AND m.event_type = 'm.room.member' \
               AND m.state_key = ac.user_id AND m.content->>'membership' = 'join' \
               AND m.redacted IS DISTINCT FROM true) \
           AND NOT EXISTS (SELECT 1 FROM room_upstream_reconcile r \
               WHERE r.account_id = $1 AND r.room_id = s.room_id AND r.state = 'gone') \
         ORDER BY l.room_id {suffix}"
    )
}

/// SQL over the clock CTE `clock` and the refresh row `r`: no backoff or
/// `unsupported` answer is in force.
const REFRESH_DUE: &str = "(r.next_attempt_at IS NULL OR r.next_attempt_at <= clock.now)";

/// The distinct `via` strings of `rows`' arrays, in order.
fn distinct_vias(rows: &[(String, Value)]) -> Vec<String> {
    let mut vias: Vec<String> = Vec::new();
    for (_, via) in rows {
        for server in via.as_array().into_iter().flatten() {
            if let Some(server) = server.as_str() {
                if !vias.iter().any(|seen| seen == server) {
                    vias.push(server.to_owned());
                }
            }
        }
    }
    vias
}

impl Store {
    /// `room_id`'s discovery row and summary refresh, whether the account may
    /// ask for a summary, and whether a read should ask now. A pure local
    /// read.
    pub async fn room_summary_snapshot(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<RoomSummarySnapshot, StoreError> {
        let sql = format!(
            "WITH clock AS (SELECT {NOW_MS} AS now) \
             SELECT (SELECT ac.state = 'active' FROM accounts ac WHERE ac.account_id = $1) \
                        IS TRUE AS active, \
                    {joined} AS joined, \
                    EXISTS ({linking}) AS linked, \
                    {discovered} AS discovered, \
                    (SELECT d.expires_at <= clock.now FROM room_discovery d \
                     WHERE d.account_id = $1 AND d.room_id = $2) AS expired, \
                    r.support, r.failure, r.next_attempt_at, \
                    COALESCE({REFRESH_DUE}, true) AS refresh_due \
             FROM clock LEFT JOIN room_discovery_refresh r \
                 ON r.account_id = $1 AND r.room_id = $2",
            joined = joined_child("$2"),
            linking = linking_spaces(""),
            discovered = discovered_json("$2"),
        );
        let row: PgRow = sqlx_core::query::query::<Postgres>(&sql)
            .bind(account_id)
            .bind(room_id)
            .fetch_one(&self.pool)
            .await?;
        let joined: bool = row.try_get("joined")?;
        let eligible =
            row.try_get::<bool, _>("active")? && !joined && row.try_get::<bool, _>("linked")?;
        let discovered = DiscoveredRow::from_json(row.try_get("discovered")?);
        // No row is unknown; a row past its expiry is stale.
        let expired: Option<bool> = row.try_get("expired")?;
        let refresh_due: bool = row.try_get("refresh_due")?;
        let support: Option<String> = row.try_get("support")?;
        let refresh = support.map(|support| -> Result<_, StoreError> {
            Ok(SummaryRefreshRow {
                support,
                failure: row.try_get("failure")?,
                next_attempt_at: row.try_get("next_attempt_at")?,
            })
        });
        let refresh = refresh.transpose()?;
        let unsupported = eligible
            && !refresh_due
            && refresh
                .as_ref()
                .is_some_and(|refresh| refresh.support == "unsupported");
        Ok(RoomSummarySnapshot {
            eligible,
            due: eligible && expired.unwrap_or(true) && refresh_due,
            joined,
            unsupported,
            discovered,
            refresh,
        })
    }

    /// Where a summary job for `room_id` starts: the `via` servers to ask
    /// through, while the room qualifies and its summary is due. Checked
    /// again here because a duplicate request can queue while an earlier
    /// job commits.
    pub async fn begin_room_summary(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<SummaryBegin, StoreError> {
        let snapshot = self.room_summary_snapshot(account_id, room_id).await?;
        if !snapshot.eligible {
            return Ok(SummaryBegin::Ineligible);
        }
        if !snapshot.due {
            return Ok(SummaryBegin::NotDue);
        }
        let rows: Vec<(String, Value)> = sqlx_core::query_as::query_as(&linking_spaces(""))
            .bind(account_id)
            .bind(room_id)
            .fetch_all(&self.pool)
            .await?;
        if rows.is_empty() {
            return Ok(SummaryBegin::Ineligible);
        }
        Ok(SummaryBegin::Request {
            vias: distinct_vias(&rows),
        })
    }

    /// Record how one summary request for `room_id` ended, if the account is
    /// still active, has still not joined the room, and a joined space still
    /// links it. One transaction; nothing is held across a request.
    pub async fn record_room_summary(
        &self,
        account_id: Uuid,
        room_id: &str,
        limits: SummaryLimits,
        step: &SummaryStep,
    ) -> Result<SummaryWrite, StoreError> {
        let mut tx = self.reconciliation_transaction().await?;
        let fenced = SummaryWrite {
            fenced: true,
            ..SummaryWrite::default()
        };
        // Share locks on every linking space's summary row: a leave of one
        // waits for this commit and its cleanup then removes what it wrote.
        let spaces: Vec<(String, Value)> =
            sqlx_core::query_as::query_as(&linking_spaces("FOR SHARE OF s"))
                .bind(account_id)
                .bind(room_id)
                .fetch_all(&mut *tx)
                .await?;
        if spaces.is_empty() {
            return Ok(fenced);
        }
        // The room's discovery row, then its refresh row: the order the
        // membership-epoch trigger and a crawl's orphan step delete them in,
        // so a join that already holds the discovery row is waited for here
        // rather than deadlocked with.
        sqlx_core::query::query(
            "SELECT 1 FROM room_discovery WHERE account_id = $1 AND room_id = $2 FOR UPDATE",
        )
        .bind(account_id)
        .bind(room_id)
        .fetch_optional(&mut *tx)
        .await?;
        let prior: Option<PgRow> = sqlx_core::query::query::<Postgres>(
            "SELECT support, failure, failures FROM room_discovery_refresh \
             WHERE account_id = $1 AND room_id = $2 FOR UPDATE",
        )
        .bind(account_id)
        .bind(room_id)
        .fetch_optional(&mut *tx)
        .await?;
        // A join committed before this result wins. Checked after the locks,
        // so a join that held them is seen.
        let (joined,): (bool,) =
            sqlx_core::query_as::query_as(&format!("SELECT {}", joined_child("$2")))
                .bind(account_id)
                .bind(room_id)
                .fetch_one(&mut *tx)
                .await?;
        if joined {
            return Ok(fenced);
        }
        let now = Self::now(&mut tx).await?;
        let (prior_support, prior_failure, prior_failures) = match &prior {
            Some(row) => (
                row.try_get::<String, _>("support")?,
                row.try_get::<Option<String>, _>("failure")?,
                row.try_get::<i32, _>("failures")?,
            ),
            None => ("unknown".to_owned(), None, 0),
        };
        let mut write = SummaryWrite::default();
        let (support, failure, failures, next_attempt_at) = match step {
            SummaryStep::Answer(entry) => {
                write.changed = Self::write_summary(
                    &mut tx,
                    account_id,
                    room_id,
                    "available",
                    Some(&entry.summary),
                    &entry.withheld,
                    now,
                    now.saturating_add(limits.summary_ttl_ms),
                )
                .await?;
                ("supported", None, 0, None)
            }
            SummaryStep::Denied { status, ttl_ms } => {
                write.changed = Self::write_summary(
                    &mut tx,
                    account_id,
                    room_id,
                    status,
                    None,
                    &[],
                    now,
                    now.saturating_add(*ttl_ms),
                )
                .await?;
                ("supported", None, 0, None)
            }
            SummaryStep::Unsupported { ttl_ms } => {
                ("unsupported", None, 0, Some(now.saturating_add(*ttl_ms)))
            }
            SummaryStep::Failure {
                category,
                retry_after_ms,
            } => {
                let failures = prior_failures.saturating_add(1);
                let delay = backoff_ms(failures, limits.min_backoff_ms, limits.max_backoff_ms)
                    .max(retry_after_ms.unwrap_or(0));
                // A failure keeps what was known about the endpoint: an
                // `unsupported` room keeps its crawl fallback through the
                // backoff, and its next attempt checks the endpoint again.
                (
                    prior_support.as_str(),
                    Some(*category),
                    failures,
                    Some(now.saturating_add(delay)),
                )
            }
        };
        write.failures = failures;
        write.changed |= prior_support != support || prior_failure.as_deref() != failure;
        sqlx_core::query::query(
            "INSERT INTO room_discovery_refresh (account_id, room_id, support, failure, \
                 failures, next_attempt_at) VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (account_id, room_id) DO UPDATE SET support = EXCLUDED.support, \
                 failure = EXCLUDED.failure, failures = EXCLUDED.failures, \
                 next_attempt_at = EXCLUDED.next_attempt_at",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(support)
        .bind(failure)
        .bind(failures)
        .bind(next_attempt_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(write)
    }

    /// Replace `room_id`'s discovery row with a summary answer; whether a read
    /// would see a different status, source, summary, or withheld list.
    #[allow(clippy::too_many_arguments)]
    async fn write_summary(
        tx: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
        account_id: Uuid,
        room_id: &str,
        status: &str,
        summary: Option<&Value>,
        withheld: &[&str],
        now: i64,
        expires_at: i64,
    ) -> Result<bool, StoreError> {
        let (changed,): (bool,) = sqlx_core::query_as::query_as(
            "WITH prior AS MATERIALIZED ( \
                 SELECT status, source, summary, withheld FROM room_discovery \
                 WHERE account_id = $1 AND room_id = $2 FOR UPDATE \
             ), written AS ( \
                 INSERT INTO room_discovery (account_id, room_id, status, source, summary, \
                     withheld, observed_at, expires_at) \
                 VALUES ($1, $2, $3, 'summary', $4, $5, $6, $7) \
                 ON CONFLICT (account_id, room_id) DO UPDATE SET status = EXCLUDED.status, \
                     source = EXCLUDED.source, summary = EXCLUDED.summary, \
                     withheld = EXCLUDED.withheld, observed_at = EXCLUDED.observed_at, \
                     expires_at = EXCLUDED.expires_at \
                 RETURNING room_id \
             ) SELECT NOT EXISTS (SELECT 1 FROM prior p WHERE p.status = $3 \
                 AND p.source = 'summary' AND p.summary IS NOT DISTINCT FROM $4 \
                 AND p.withheld = $5)",
        )
        .bind(account_id)
        .bind(room_id)
        .bind(status)
        .bind(summary)
        .bind(withheld)
        .bind(now)
        .bind(expires_at)
        .fetch_one(&mut **tx)
        .await?;
        Ok(changed)
    }

    /// The first joined space, in room id order, that links `room_id` and
    /// whose crawl is due (ADR 0128 decision 2), for a room whose summary
    /// endpoint is unsupported: its summary can come only from a crawl of a
    /// linking space (decision 5). A pure local read.
    pub async fn room_summary_fallback_space(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<Option<String>, StoreError> {
        let sql = format!(
            "WITH clock AS (SELECT {NOW_MS} AS now), spaces AS ({linking}) \
             SELECT spaces.space_id FROM spaces, clock \
             WHERE NOT EXISTS (SELECT 1 FROM space_discovery c \
                 WHERE c.account_id = $1 AND c.space_id = spaces.space_id \
                   AND CASE WHEN c.outcome IN ('complete', 'truncated') \
                            THEN c.expires_at > clock.now \
                            ELSE c.next_attempt_at > clock.now END) \
             ORDER BY spaces.space_id LIMIT 1",
            linking = linking_spaces(""),
        );
        let space: Option<(String,)> = sqlx_core::query_as::query_as(&sql)
            .bind(account_id)
            .bind(room_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(space.map(|(space,)| space))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn vias_are_distinct_in_space_then_link_order() {
        let rows = vec![
            ("!a:x".to_owned(), json!(["one.org", "two.org"])),
            ("!b:x".to_owned(), json!(["two.org", 7, "three.org"])),
            ("!c:x".to_owned(), json!("not an array")),
        ];
        assert_eq!(distinct_vias(&rows), ["one.org", "two.org", "three.org"]);
    }
}
