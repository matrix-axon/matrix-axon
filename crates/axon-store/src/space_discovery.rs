//! Account-scoped cache of unjoined rooms discovered through space
//! hierarchies (ADR 0128 decisions 3, 4, and 6).
//!
//! `room_discovery` holds one summary per account and room, shared by every
//! space that links the room; `space_discovery` holds each space's crawl; and
//! `space_discovery_children` records which children each crawl generation
//! returned. None of it is merged into `room_state` or `room_summaries`, and
//! the membership triggers in the migration delete it on leave, purge, and
//! account removal. Every time here is Unix milliseconds from the PostgreSQL
//! clock.
//!
//! Lock order, shared with ADR 0126's acquisition writes: a share lock on the
//! space's `room_summaries` row, then the crawl row, then `room_discovery`
//! rows in room id order. A leave updates the summary row before its trigger
//! deletes discovery rows, so it waits for a page commit in flight and then
//! removes what that commit wrote.
use serde_json::Value;
use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::room_detail_acquisition::{backoff_ms, JOINED_EPOCH, NOW_MS};
use crate::spaces::SPACE_LINK_IS_CURRENT;
use crate::{Store, StoreError};

/// One cached discovery row as a read sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredRow {
    /// `available`, `not_returned`, `forbidden`, or `not_found`.
    pub status: String,
    /// `hierarchy` or `summary`.
    pub source: String,
    /// The validated fields, as a JSON object; `None` unless `available`.
    pub summary: Option<Value>,
    /// Fields the latest answer carried that failed validation.
    pub withheld: Vec<String>,
    pub observed_at: i64,
    pub expires_at: i64,
}

impl DiscoveredRow {
    /// The row a read's `jsonb_build_object` produced, or `None` for no row.
    pub(crate) fn from_json(value: Option<Value>) -> Option<Self> {
        let value = value?;
        let text = |key: &str| value.get(key)?.as_str().map(str::to_owned);
        let int = |key: &str| value.get(key)?.as_i64();
        Some(DiscoveredRow {
            status: text("status")?,
            source: text("source")?,
            summary: value.get("summary").filter(|v| v.is_object()).cloned(),
            withheld: value
                .get("withheld")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            observed_at: int("observed_at")?,
            expires_at: int("expires_at")?,
        })
    }
}

/// SQL expression, over a child room id `child`, that reads its discovery row
/// as the JSON object [`DiscoveredRow::from_json`] parses. `$1` is the account.
pub(crate) fn discovered_json(child: &str) -> String {
    format!(
        "(SELECT jsonb_build_object('status', d.status, 'source', d.source, \
                 'summary', d.summary, 'withheld', to_jsonb(d.withheld), \
                 'observed_at', d.observed_at, 'expires_at', d.expires_at) \
          FROM room_discovery d WHERE d.account_id = $1 AND d.room_id = {child})"
    )
}

/// A space's crawl as a read sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceCrawlRow {
    pub generation: i64,
    /// `in_progress`, `complete`, `truncated`, or `failed`.
    pub outcome: String,
    pub pages: i32,
    pub page_limit: i32,
    pub rooms_cached: i32,
    pub started_at: i64,
    pub completed_at: Option<i64>,
    pub expires_at: Option<i64>,
    /// `unknown`, `supported`, or `unsupported`.
    pub support: String,
    pub failure: Option<String>,
    pub next_attempt_at: Option<i64>,
}

/// A space's crawl and whether a read with `enrich` may schedule one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceCrawlSnapshot {
    /// The account is active and joined to the space.
    pub eligible: bool,
    /// Eligible, and the crawl is absent, ended and expired, or unfinished
    /// and out of backoff (decision 2).
    pub due: bool,
    pub crawl: Option<SpaceCrawlRow>,
}

/// Where a crawl job starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrawlBegin {
    /// Request this page.
    Page(CrawlPosition),
    /// The crawl ended and has not expired, or is backing off.
    NotDue,
    /// The account is no longer an active member of the space in `epoch`.
    Fenced,
}

/// The page a job requests, and the generation it commits to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrawlPosition {
    pub generation: i64,
    /// The `from` token; `None` for the first page.
    pub from: Option<String>,
    pub page_limit: i32,
    /// Pages this generation has committed.
    pub pages: i32,
    /// Restarts this crawl has made after an expired cursor.
    pub restarts: i32,
}

/// One validated hierarchy entry.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredEntry {
    pub room_id: String,
    /// The validated fields, as a JSON object.
    pub summary: Value,
    /// Fields that failed validation, by name.
    pub withheld: Vec<&'static str>,
}

/// The limits a crawl applies (ADR 0128 decision 10), passed in by the
/// worker that owns them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrawlLimits {
    /// The page limit a crawl starts at.
    pub page_limit: i32,
    pub max_pages: i32,
    pub max_cursor_restarts: i32,
    pub crawl_ttl_ms: i64,
    /// ADR 0126's failure backoff: the first failure's delay, doubling per
    /// consecutive failure up to the maximum.
    pub min_backoff_ms: i64,
    pub max_backoff_ms: i64,
}

/// How one hierarchy request ended, as the crawl records it.
#[derive(Debug, Clone, PartialEq)]
pub enum CrawlStep {
    /// A parsed page.
    Page {
        entries: Vec<DiscoveredEntry>,
        /// Entries skipped whole: no valid room id, or not an object.
        unidentified: i32,
        next_batch: Option<String>,
    },
    /// The body exceeded the byte limit. The crawl's page limit halves and
    /// the same page is asked again; at a limit of one the crawl ends
    /// `truncated` with `too_large`.
    TooLarge,
    /// `400 M_INVALID_PARAM` on a resumed page: the cursor's session expired.
    CursorExpired,
    /// `403` or `404 M_NOT_FOUND`: `forbidden` or `not_found`.
    Denied { failure: &'static str, ttl_ms: i64 },
    /// The homeserver lacks the endpoint.
    Unsupported { ttl_ms: i64 },
    /// A transient failure: ADR 0126's backoff, and never sooner than
    /// `retry_after_ms`. Committed pages and the cursor are kept.
    Failure {
        category: &'static str,
        retry_after_ms: Option<i64>,
    },
}

/// What recording a step did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrawlWrite {
    /// The membership epoch or the crawl's generation moved on; nothing was
    /// written.
    pub fenced: bool,
    /// At least one cached summary a read shows changed.
    pub changed: bool,
    /// The crawl reached `complete`, `truncated`, or a failure.
    pub conclusive: bool,
    /// Another page is due now: queue it.
    pub next_page: bool,
    /// The crawl's outcome after the write.
    pub outcome: &'static str,
    /// The failure category recorded, if any.
    pub failure: Option<&'static str>,
    /// The page limit after the write.
    pub page_limit: i32,
    /// Consecutive transient failures after the write.
    pub failures: i32,
    /// Linked, unjoined entries cached by this page.
    pub cached: usize,
    /// Entries for rooms the space has no current link to; dropped.
    pub unlinked: usize,
    /// Linked entries skipped because the account has joined the child.
    pub joined: usize,
    /// Children marked `not_returned` by a complete crawl.
    pub omitted: usize,
    /// Discovery rows deleted because no joined space links them.
    pub orphans: usize,
    /// Omission was skipped because the generation skipped an entry.
    pub omission_skipped: bool,
    /// The crawl restarted from the first page.
    pub restarted: bool,
}

/// The crawl row a write starts from.
struct Crawl {
    generation: i64,
    outcome: String,
    cursor: Option<String>,
    tokens: Vec<String>,
    pages: i32,
    page_limit: i32,
    restarts: i32,
    unidentified: i32,
    started_at: i64,
    failures: i32,
}

const CRAWL_COLUMNS: &str = "generation, outcome, cursor, tokens, pages, page_limit, restarts, \
     unidentified, started_at, failures";

fn crawl_from_row(row: &PgRow) -> Result<Crawl, sqlx_core::Error> {
    Ok(Crawl {
        generation: row.try_get("generation")?,
        outcome: row.try_get("outcome")?,
        cursor: row.try_get("cursor")?,
        tokens: row.try_get("tokens")?,
        pages: row.try_get("pages")?,
        page_limit: row.try_get("page_limit")?,
        restarts: row.try_get("restarts")?,
        unidentified: row.try_get("unidentified")?,
        started_at: row.try_get("started_at")?,
        failures: row.try_get("failures")?,
    })
}

/// SQL predicate, over a child id expression `child`, that the account's own
/// membership in it is `join`. The hierarchy read's `joined` and the crawl's
/// joined filter both use it, so they cannot disagree. `$1` is the account.
pub(crate) fn joined_child(child: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM room_state m \
           WHERE m.account_id = $1 AND m.room_id = {child} \
             AND m.event_type = 'm.room.member' \
             AND m.state_key = (SELECT ac.user_id FROM accounts ac WHERE ac.account_id = $1) \
             AND m.content->>'membership' = 'join' \
             AND m.redacted IS DISTINCT FROM true)"
    )
}

/// SQL predicate, over a child id expression `child`, that space `$2` links
/// it with a current `m.space.child`. `$1` is the account.
fn linked_child(child: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM room_state \
           WHERE account_id = $1 AND room_id = $2 AND event_type = 'm.space.child' \
             AND state_key = {child} AND {SPACE_LINK_IS_CURRENT})"
    )
}

impl Store {
    /// `space_id`'s crawl, whether the account may crawl it, and whether a
    /// read with `enrich` should ask for a crawl now. A pure local read.
    pub async fn space_crawl_snapshot(
        &self,
        account_id: Uuid,
        space_id: &str,
    ) -> Result<SpaceCrawlSnapshot, StoreError> {
        let sql = format!(
            "WITH clock AS (SELECT {NOW_MS} AS now) \
             SELECT generation, outcome, pages, page_limit, rooms_cached, started_at, \
                    completed_at, expires_at, support, failure, next_attempt_at, \
                    CASE WHEN outcome IN ('complete', 'truncated') \
                         THEN expires_at IS NULL OR expires_at <= clock.now \
                         ELSE next_attempt_at IS NULL OR next_attempt_at <= clock.now END AS due \
             FROM space_discovery, clock WHERE account_id = $1 AND space_id = $2"
        );
        let row: Option<PgRow> = sqlx_core::query::query::<Postgres>(&sql)
            .bind(account_id)
            .bind(space_id)
            .fetch_optional(&self.pool)
            .await?;
        let (crawl, due) = match row {
            None => (None, true),
            Some(row) => (
                Some(SpaceCrawlRow {
                    generation: row.try_get("generation")?,
                    outcome: row.try_get("outcome")?,
                    pages: row.try_get("pages")?,
                    page_limit: row.try_get("page_limit")?,
                    rooms_cached: row.try_get("rooms_cached")?,
                    started_at: row.try_get("started_at")?,
                    completed_at: row.try_get("completed_at")?,
                    expires_at: row.try_get("expires_at")?,
                    support: row.try_get("support")?,
                    failure: row.try_get("failure")?,
                    next_attempt_at: row.try_get("next_attempt_at")?,
                }),
                row.try_get::<bool, _>("due")?,
            ),
        };
        let eligible = self.joined_epoch(account_id, space_id).await?.is_some();
        Ok(SpaceCrawlSnapshot {
            eligible,
            due: eligible && due,
            crawl,
        })
    }

    /// Where a crawl job for `space_id` starts: the saved cursor of an
    /// unfinished crawl, or the first page of a new generation when the last
    /// crawl ended and expired or left no cursor. Only while the account is
    /// an active member of the space in `epoch`.
    pub async fn begin_space_crawl(
        &self,
        account_id: Uuid,
        space_id: &str,
        epoch: i64,
        limits: CrawlLimits,
    ) -> Result<CrawlBegin, StoreError> {
        let mut tx = self.reconciliation_transaction().await?;
        if !Self::fence(&mut tx, account_id, space_id, epoch).await? {
            return Ok(CrawlBegin::Fenced);
        }
        let now = Self::now(&mut tx).await?;
        let sql = format!(
            "SELECT {CRAWL_COLUMNS}, completed_at, expires_at, next_attempt_at \
             FROM space_discovery WHERE account_id = $1 AND space_id = $2 FOR UPDATE"
        );
        let row: Option<PgRow> = sqlx_core::query::query::<Postgres>(&sql)
            .bind(account_id)
            .bind(space_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            sqlx_core::query::query(
                "INSERT INTO space_discovery (account_id, space_id, generation, outcome, \
                     page_limit, started_at) VALUES ($1, $2, 1, 'in_progress', $3, $4)",
            )
            .bind(account_id)
            .bind(space_id)
            .bind(limits.page_limit)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(CrawlBegin::Page(CrawlPosition {
                generation: 1,
                from: None,
                page_limit: limits.page_limit,
                pages: 0,
                restarts: 0,
            }));
        };
        let crawl = crawl_from_row(&row)?;
        let expires_at: Option<i64> = row.try_get("expires_at")?;
        let next_attempt_at: Option<i64> = row.try_get("next_attempt_at")?;
        let resume = |crawl: &Crawl| {
            CrawlBegin::Page(CrawlPosition {
                generation: crawl.generation,
                from: crawl.cursor.clone(),
                page_limit: crawl.page_limit,
                pages: crawl.pages,
                restarts: crawl.restarts,
            })
        };
        let begin = match crawl.outcome.as_str() {
            "in_progress" => resume(&crawl),
            "failed" if next_attempt_at.is_some_and(|at| at > now) => CrawlBegin::NotDue,
            "failed" if crawl.cursor.is_some() => {
                // Out of backoff: the same generation goes on from its
                // cursor. The failure category stays until a page succeeds.
                sqlx_core::query::query(
                    "UPDATE space_discovery SET outcome = 'in_progress', expires_at = NULL, \
                         next_attempt_at = NULL \
                     WHERE account_id = $1 AND space_id = $2",
                )
                .bind(account_id)
                .bind(space_id)
                .execute(&mut *tx)
                .await?;
                resume(&crawl)
            }
            "complete" | "truncated" if expires_at.is_none_or(|at| at > now) => CrawlBegin::NotDue,
            // Ended and expired, or failed with nothing to resume from.
            _ => {
                Self::new_generation(&mut tx, account_id, space_id, now, limits.page_limit, 0)
                    .await?;
                CrawlBegin::Page(CrawlPosition {
                    generation: crawl.generation + 1,
                    from: None,
                    page_limit: limits.page_limit,
                    pages: 0,
                    restarts: 0,
                })
            }
        };
        tx.commit().await?;
        Ok(begin)
    }

    /// Record how one hierarchy request of `generation` ended, if the
    /// account is still an active member of the space in `epoch` and the
    /// crawl is still that generation and unfinished.
    ///
    /// A page caches each entry the space links and the account has not
    /// joined, records the returned set, and moves the cursor; a page with no
    /// `next_batch` completes the crawl, which marks unreturned children
    /// `not_returned` and deletes discovery rows no joined space links (ADR
    /// 0128 decision 3). One transaction; nothing is held across a request.
    pub async fn record_space_crawl(
        &self,
        account_id: Uuid,
        space_id: &str,
        epoch: i64,
        generation: i64,
        limits: CrawlLimits,
        step: &CrawlStep,
    ) -> Result<CrawlWrite, StoreError> {
        let mut tx = self.reconciliation_transaction().await?;
        let fenced = CrawlWrite {
            fenced: true,
            ..CrawlWrite::default()
        };
        if !Self::fence(&mut tx, account_id, space_id, epoch).await? {
            return Ok(fenced);
        }
        let sql = format!(
            "SELECT {CRAWL_COLUMNS} FROM space_discovery \
             WHERE account_id = $1 AND space_id = $2 FOR UPDATE"
        );
        let row: Option<PgRow> = sqlx_core::query::query::<Postgres>(&sql)
            .bind(account_id)
            .bind(space_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(crawl) = row.as_ref().map(crawl_from_row).transpose()? else {
            return Ok(fenced);
        };
        if crawl.generation != generation || crawl.outcome != "in_progress" {
            return Ok(fenced);
        }
        let now = Self::now(&mut tx).await?;
        let mut write = CrawlWrite {
            page_limit: crawl.page_limit,
            ..CrawlWrite::default()
        };
        match step {
            CrawlStep::Page {
                entries,
                unidentified,
                next_batch,
            } => {
                self.record_page(
                    &mut tx,
                    account_id,
                    space_id,
                    &crawl,
                    limits,
                    now,
                    entries,
                    *unidentified,
                    next_batch.as_deref(),
                    &mut write,
                )
                .await?;
            }
            CrawlStep::TooLarge if crawl.page_limit > 1 => {
                // Never raised again within this crawl, so a crawl makes at
                // most log2(PAGE_LIMIT) + 1 size retries in all. Re-asking a
                // resumed page with a smaller `limit` keeps its `from`:
                // Synapse binds a token to the room, `max_depth`, and
                // `suggested_only` only, and reads `limit` per request.
                write.page_limit = crawl.page_limit / 2;
                write.outcome = "in_progress";
                write.next_page = true;
                sqlx_core::query::query(
                    "UPDATE space_discovery SET page_limit = $3, support = 'supported' \
                     WHERE account_id = $1 AND space_id = $2",
                )
                .bind(account_id)
                .bind(space_id)
                .bind(write.page_limit)
                .execute(&mut *tx)
                .await?;
            }
            CrawlStep::TooLarge => {
                Self::end_crawl(
                    &mut tx,
                    account_id,
                    space_id,
                    now,
                    "truncated",
                    Some("too_large"),
                    limits.crawl_ttl_ms,
                    &mut write,
                )
                .await?;
            }
            CrawlStep::CursorExpired if crawl.restarts < limits.max_cursor_restarts => {
                // The lower page limit is the same crawl's; keep it.
                Self::new_generation(
                    &mut tx,
                    account_id,
                    space_id,
                    now,
                    crawl.page_limit,
                    crawl.restarts + 1,
                )
                .await?;
                write.outcome = "in_progress";
                write.next_page = true;
                write.restarted = true;
            }
            CrawlStep::CursorExpired => {
                // An expired cursor cannot be resumed; the attempt after the
                // backoff starts a new generation.
                let delay = backoff_ms(
                    crawl.failures.saturating_add(1),
                    limits.min_backoff_ms,
                    limits.max_backoff_ms,
                );
                Self::fail_crawl(
                    &mut tx,
                    account_id,
                    space_id,
                    now,
                    &crawl,
                    "cursor_expired",
                    delay,
                    true,
                    &mut write,
                )
                .await?;
            }
            CrawlStep::Denied { failure, ttl_ms } => {
                // Conclusive: no backoff growth, and no cursor to resume.
                // Cached summaries stay until a complete crawl or a leave.
                sqlx_core::query::query(
                    "UPDATE space_discovery SET outcome = 'failed', cursor = NULL, \
                         failure = $3, failures = 0, support = 'supported', \
                         expires_at = $4, next_attempt_at = $4 \
                     WHERE account_id = $1 AND space_id = $2",
                )
                .bind(account_id)
                .bind(space_id)
                .bind(*failure)
                .bind(now.saturating_add(*ttl_ms))
                .execute(&mut *tx)
                .await?;
                write.outcome = "failed";
                write.failure = Some(failure);
                write.conclusive = true;
            }
            CrawlStep::Unsupported { ttl_ms } => {
                sqlx_core::query::query(
                    "UPDATE space_discovery SET outcome = 'failed', cursor = NULL, \
                         failure = NULL, failures = 0, support = 'unsupported', \
                         expires_at = $3, next_attempt_at = $3 \
                     WHERE account_id = $1 AND space_id = $2",
                )
                .bind(account_id)
                .bind(space_id)
                .bind(now.saturating_add(*ttl_ms))
                .execute(&mut *tx)
                .await?;
                write.outcome = "failed";
                write.conclusive = true;
            }
            CrawlStep::Failure {
                category,
                retry_after_ms,
            } => {
                let delay = backoff_ms(
                    crawl.failures.saturating_add(1),
                    limits.min_backoff_ms,
                    limits.max_backoff_ms,
                )
                .max(retry_after_ms.unwrap_or(0));
                Self::fail_crawl(
                    &mut tx, account_id, space_id, now, &crawl, category, delay, false, &mut write,
                )
                .await?;
            }
        }
        tx.commit().await?;
        Ok(write)
    }

    /// The share lock and membership check every crawl write starts with.
    async fn fence(
        tx: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
        account_id: Uuid,
        space_id: &str,
        epoch: i64,
    ) -> Result<bool, StoreError> {
        let current: Option<(i64,)> =
            sqlx_core::query_as::query_as(&format!("{JOINED_EPOCH} FOR SHARE OF s"))
                .bind(account_id)
                .bind(space_id)
                .fetch_optional(&mut **tx)
                .await?;
        Ok(current == Some((epoch,)))
    }

    async fn now(
        tx: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
    ) -> Result<i64, StoreError> {
        let (now,): (i64,) = sqlx_core::query_as::query_as(&format!("SELECT {NOW_MS}"))
            .fetch_one(&mut **tx)
            .await?;
        Ok(now)
    }

    /// Start a new generation from the first page. Outcomes, support, and
    /// the failure count carry over; the returned set restarts.
    async fn new_generation(
        tx: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
        account_id: Uuid,
        space_id: &str,
        now: i64,
        page_limit: i32,
        restarts: i32,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query(
            "UPDATE space_discovery SET generation = generation + 1, outcome = 'in_progress', \
                 cursor = NULL, tokens = '{}', pages = 0, page_limit = $3, restarts = $4, \
                 unidentified = 0, rooms_cached = 0, started_at = $5, completed_at = NULL, \
                 expires_at = NULL, next_attempt_at = NULL \
             WHERE account_id = $1 AND space_id = $2",
        )
        .bind(account_id)
        .bind(space_id)
        .bind(page_limit)
        .bind(restarts)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// End the crawl `complete` or `truncated` until the crawl TTL passes.
    #[allow(clippy::too_many_arguments)]
    async fn end_crawl(
        tx: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
        account_id: Uuid,
        space_id: &str,
        now: i64,
        outcome: &'static str,
        failure: Option<&'static str>,
        crawl_ttl_ms: i64,
        write: &mut CrawlWrite,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query(
            "UPDATE space_discovery SET outcome = $3, failure = $4, cursor = NULL, \
                 completed_at = $5, expires_at = $6, next_attempt_at = NULL, \
                 support = 'supported', failures = 0 \
             WHERE account_id = $1 AND space_id = $2",
        )
        .bind(account_id)
        .bind(space_id)
        .bind(outcome)
        .bind(failure)
        .bind(now)
        .bind(now.saturating_add(crawl_ttl_ms))
        .execute(&mut **tx)
        .await?;
        write.outcome = outcome;
        write.failure = failure;
        write.conclusive = true;
        Ok(())
    }

    /// Make the crawl `failed` until `delay_ms` passes, keeping the cursor
    /// and the committed pages unless `drop_cursor`.
    #[allow(clippy::too_many_arguments)]
    async fn fail_crawl(
        tx: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
        account_id: Uuid,
        space_id: &str,
        now: i64,
        crawl: &Crawl,
        category: &'static str,
        delay_ms: i64,
        drop_cursor: bool,
        write: &mut CrawlWrite,
    ) -> Result<(), StoreError> {
        let failures = crawl.failures.saturating_add(1);
        sqlx_core::query::query(
            "UPDATE space_discovery SET outcome = 'failed', failure = $3, failures = $4, \
                 next_attempt_at = $5, cursor = CASE WHEN $6 THEN NULL ELSE cursor END \
             WHERE account_id = $1 AND space_id = $2",
        )
        .bind(account_id)
        .bind(space_id)
        .bind(category)
        .bind(failures)
        .bind(now.saturating_add(delay_ms))
        .bind(drop_cursor)
        .execute(&mut **tx)
        .await?;
        write.outcome = "failed";
        write.failure = Some(category);
        write.failures = failures;
        write.conclusive = true;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn record_page(
        &self,
        tx: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
        account_id: Uuid,
        space_id: &str,
        crawl: &Crawl,
        limits: CrawlLimits,
        now: i64,
        entries: &[DiscoveredEntry],
        unidentified: i32,
        next_batch: Option<&str>,
        write: &mut CrawlWrite,
    ) -> Result<(), StoreError> {
        // Duplicate ids in one page keep the first entry. A page is bounded
        // by the worker's body limit, not by its page limit, so lookups stay
        // linear in its size.
        let mut by_id: std::collections::HashMap<&str, &DiscoveredEntry> =
            std::collections::HashMap::with_capacity(entries.len());
        for entry in entries {
            if entry.room_id != space_id {
                by_id.entry(entry.room_id.as_str()).or_insert(entry);
            }
        }
        let ids: Vec<&str> = by_id.keys().copied().collect();
        let sql = format!(
            "SELECT i.id, {linked} AS linked, {joined} AS joined \
             FROM unnest($3::text[]) AS i(id)",
            linked = linked_child("i.id"),
            joined = joined_child("i.id"),
        );
        let membership: Vec<(String, bool, bool)> = sqlx_core::query_as::query_as(&sql)
            .bind(account_id)
            .bind(space_id)
            .bind(&ids)
            .fetch_all(&mut **tx)
            .await?;
        let mut returned: Vec<&str> = Vec::new();
        let mut cache: Vec<&DiscoveredEntry> = Vec::new();
        for (id, linked, joined) in &membership {
            let Some(entry) = by_id.get(id.as_str()).copied() else {
                continue;
            };
            if !linked {
                write.unlinked += 1;
                continue;
            }
            returned.push(entry.room_id.as_str());
            if *joined {
                // A join committed before this page wins.
                write.joined += 1;
            } else {
                cache.push(entry);
            }
        }
        cache.sort_by(|a, b| a.room_id.cmp(&b.room_id));
        write.cached = cache.len();

        if !cache.is_empty() {
            let cache_ids: Vec<&str> = cache.iter().map(|entry| entry.room_id.as_str()).collect();
            let summaries: Vec<&Value> = cache.iter().map(|entry| &entry.summary).collect();
            let withheld: Vec<Value> = cache
                .iter()
                .map(|entry| Value::from(entry.withheld.clone()))
                .collect();
            // Lock in id order before comparing, so two writers of a row
            // cannot both see the old value and both skip the change.
            let changed: (bool,) = sqlx_core::query_as::query_as(
                "WITH input AS ( \
                     SELECT i.id, i.summary, \
                            ARRAY(SELECT jsonb_array_elements_text(i.withheld)) AS withheld \
                     FROM unnest($2::text[], $3::jsonb[], $4::jsonb[]) AS i(id, summary, withheld) \
                 ), prior AS MATERIALIZED ( \
                     SELECT d.room_id, d.status, d.summary, d.withheld FROM room_discovery d \
                     WHERE d.account_id = $1 AND d.room_id = ANY($2::text[]) \
                     ORDER BY d.room_id FOR UPDATE \
                 ), written AS ( \
                     INSERT INTO room_discovery (account_id, room_id, status, source, summary, \
                         withheld, observed_at, expires_at) \
                     SELECT $1, id, 'available', 'hierarchy', summary, withheld, $5, $6 \
                     FROM input ORDER BY id \
                     ON CONFLICT (account_id, room_id) DO UPDATE SET status = EXCLUDED.status, \
                         source = EXCLUDED.source, summary = EXCLUDED.summary, \
                         withheld = EXCLUDED.withheld, observed_at = EXCLUDED.observed_at, \
                         expires_at = EXCLUDED.expires_at \
                     RETURNING room_id \
                 ) SELECT EXISTS (SELECT 1 FROM input i WHERE NOT EXISTS ( \
                     SELECT 1 FROM prior p WHERE p.room_id = i.id AND p.status = 'available' \
                       AND p.summary = i.summary AND p.withheld = i.withheld))",
            )
            .bind(account_id)
            .bind(&cache_ids)
            .bind(&summaries)
            .bind(&withheld)
            .bind(now)
            .bind(now.saturating_add(limits.crawl_ttl_ms))
            .fetch_one(&mut **tx)
            .await?;
            write.changed = changed.0;
        }

        // The returned set, counting a child once per generation.
        let newly_returned: Vec<(String,)> = sqlx_core::query_as::query_as(
            "INSERT INTO space_discovery_children (account_id, space_id, child_id, generation) \
             SELECT $1, $2, id, $4 FROM unnest($3::text[]) AS i(id) ORDER BY id \
             ON CONFLICT (account_id, space_id, child_id) DO UPDATE \
                 SET generation = EXCLUDED.generation \
                 WHERE space_discovery_children.generation <> EXCLUDED.generation \
             RETURNING child_id",
        )
        .bind(account_id)
        .bind(space_id)
        .bind(&returned)
        .bind(crawl.generation)
        .fetch_all(&mut **tx)
        .await?;

        let from = crawl.cursor.as_deref();
        let pages = crawl.pages.saturating_add(1);
        let unidentified = crawl.unidentified.saturating_add(unidentified.max(0));
        let looped = next_batch.is_some_and(|next| {
            from == Some(next) || crawl.tokens.iter().any(|token| token == next)
        });
        sqlx_core::query::query(
            "UPDATE space_discovery SET pages = $3, unidentified = $4, \
                 rooms_cached = rooms_cached + $5, cursor = $6, \
                 tokens = CASE WHEN $7::text IS NULL THEN tokens ELSE array_append(tokens, $7) END, \
                 failure = NULL, failures = 0, next_attempt_at = NULL, support = 'supported' \
             WHERE account_id = $1 AND space_id = $2",
        )
        .bind(account_id)
        .bind(space_id)
        .bind(pages)
        .bind(unidentified)
        .bind(i32::try_from(newly_returned.len()).unwrap_or(i32::MAX))
        .bind(next_batch)
        .bind(from)
        .execute(&mut **tx)
        .await?;

        match next_batch {
            None => {
                if unidentified == 0 {
                    write.omitted = self
                        .omit_unreturned(tx, account_id, space_id, crawl, now, limits)
                        .await?;
                    write.changed |= write.omitted > 0;
                } else {
                    write.omission_skipped = true;
                }
                write.orphans = Self::delete_orphans(tx, account_id, space_id).await?;
                Self::end_crawl(
                    tx,
                    account_id,
                    space_id,
                    now,
                    "complete",
                    None,
                    limits.crawl_ttl_ms,
                    write,
                )
                .await?;
            }
            Some(_) if looped => {
                Self::end_crawl(
                    tx,
                    account_id,
                    space_id,
                    now,
                    "truncated",
                    Some("pagination_loop"),
                    limits.crawl_ttl_ms,
                    write,
                )
                .await?;
            }
            Some(_) if pages >= limits.max_pages => {
                Self::end_crawl(
                    tx,
                    account_id,
                    space_id,
                    now,
                    "truncated",
                    None,
                    limits.crawl_ttl_ms,
                    write,
                )
                .await?;
            }
            Some(_) => {
                write.outcome = "in_progress";
                write.next_page = true;
            }
        }
        Ok(())
    }

    /// Mark `not_returned` every linked child this generation did not
    /// return whose summary predates the generation, so a summary another
    /// crawl or a room-summary job refreshed meanwhile is kept.
    async fn omit_unreturned(
        &self,
        tx: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
        account_id: Uuid,
        space_id: &str,
        crawl: &Crawl,
        now: i64,
        limits: CrawlLimits,
    ) -> Result<usize, StoreError> {
        let sql = format!(
            "UPDATE room_discovery d SET status = 'not_returned', source = 'hierarchy', \
                 summary = NULL, withheld = '{{}}', observed_at = $4, expires_at = $5 \
             WHERE d.account_id = $1 AND d.status = 'available' AND d.observed_at < $3 \
               AND {linked} \
               AND NOT EXISTS (SELECT 1 FROM space_discovery_children c \
                   WHERE c.account_id = $1 AND c.space_id = $2 AND c.child_id = d.room_id \
                     AND c.generation = $6) \
             RETURNING d.room_id",
            linked = linked_child("d.room_id"),
        );
        let omitted: Vec<(String,)> = sqlx_core::query_as::query_as(&sql)
            .bind(account_id)
            .bind(space_id)
            .bind(crawl.started_at)
            .bind(now)
            .bind(now.saturating_add(limits.crawl_ttl_ms))
            .bind(crawl.generation)
            .fetch_all(&mut **tx)
            .await?;
        Ok(omitted.len())
    }

    /// Delete discovery rows no joined space links any more, and this
    /// space's returned-set rows for links that no longer exist.
    async fn delete_orphans(
        tx: &mut sqlx_core::transaction::Transaction<'_, Postgres>,
        account_id: Uuid,
        space_id: &str,
    ) -> Result<usize, StoreError> {
        let orphans: Vec<(String,)> = sqlx_core::query_as::query_as(
            "DELETE FROM room_discovery d WHERE d.account_id = $1 \
               AND NOT room_discovery_is_linked(d.account_id, d.room_id) \
             RETURNING d.room_id",
        )
        .bind(account_id)
        .fetch_all(&mut **tx)
        .await?;
        let sql = format!(
            "DELETE FROM space_discovery_children c \
             WHERE c.account_id = $1 AND c.space_id = $2 AND NOT {linked}",
            linked = linked_child("c.child_id"),
        );
        sqlx_core::query::query(&sql)
            .bind(account_id)
            .bind(space_id)
            .execute(&mut **tx)
            .await?;
        Ok(orphans.len())
    }

    /// Whether `space_id` links, with a current `m.space.child`, at least one
    /// room the account has not joined: a crawl of a space whose children are
    /// all joined could discover nothing (a bridge's portal space, say). It
    /// stops at the first such child, so a mostly-unjoined space answers at
    /// once. A pure local read.
    pub async fn space_has_unjoined_children(
        &self,
        account_id: Uuid,
        space_id: &str,
    ) -> Result<bool, StoreError> {
        let sql = format!(
            "SELECT EXISTS (SELECT 1 FROM room_state \
               WHERE account_id = $1 AND room_id = $2 AND event_type = 'm.space.child' \
                 AND {SPACE_LINK_IS_CURRENT} AND NOT {joined})",
            joined = joined_child("room_state.state_key"),
        );
        let (any,): (bool,) = sqlx_core::query_as::query_as(&sql)
            .bind(account_id)
            .bind(space_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(any)
    }

    /// The discovery row for `room_id`, if any. A pure local read.
    pub async fn room_discovery(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<Option<DiscoveredRow>, StoreError> {
        let sql = format!("SELECT {} AS discovered", discovered_json("$2"));
        let (value,): (Option<Value>,) = sqlx_core::query_as::query_as(&sql)
            .bind(account_id)
            .bind(room_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(DiscoveredRow::from_json(value))
    }
}
