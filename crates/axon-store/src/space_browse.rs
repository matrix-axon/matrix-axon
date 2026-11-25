//! Cache and local read projections for explicit nested space discovery.

use std::time::Duration;

use serde_json::Value;
use sqlx_core::row::Row;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::Postgres;
use uuid::Uuid;

use crate::room_detail_acquisition::JOINED_EPOCH;
use crate::space_discovery::{joined_child, DiscoveredEntry, DiscoveredRow};
use crate::space_graph::visible_joined_room;
use crate::spaces::{synced_space, NORMALIZED_ORDER, SPACE_LINK_IS_CURRENT};
use crate::{Store, StoreError};

pub const SPACE_BROWSE_MAX_DEPTH: i32 = 3;
pub const SPACE_BROWSE_TTL: Duration = Duration::from_secs(6 * 60 * 60);
pub const SPACE_BROWSE_MAX_PAGE: i64 = 100;

const EXPIRED_ROOTS_PER_SWEEP: i64 = 16;

pub(crate) fn browse_root() -> String {
    format!("NOT s.hidden_tombstoned AND {}", synced_space("s.room_id"))
}

#[derive(Debug)]
pub struct SpaceBrowseLink {
    pub parent_room_id: String,
    pub child_room_id: String,
    pub joined: bool,
    pub order: Option<String>,
    pub origin_ts: i64,
    pub suggested: bool,
    pub discovered: Option<DiscoveredRow>,
}

#[derive(Debug)]
pub struct DiscoveredSpaceMembership {
    pub space_id: String,
    pub room_id: String,
    pub expires_at: i64,
}

impl Store {
    pub(crate) async fn can_browse_space(
        &self,
        account_id: Uuid,
        space_id: &str,
    ) -> Result<bool, StoreError> {
        let row: Option<(i64,)> =
            sqlx_core::query_as::query_as(&format!("{JOINED_EPOCH} AND {}", browse_root()))
                .bind(account_id)
                .bind(space_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.is_some())
    }

    pub(crate) async fn clear_space_browse(
        tx: &mut Transaction<'_, Postgres>,
        account_id: Uuid,
        space_id: &str,
    ) -> Result<(), StoreError> {
        sqlx_core::query::query(
            "DELETE FROM space_browse_rooms WHERE account_id = $1 AND space_id = $2",
        )
        .bind(account_id)
        .bind(space_id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Clear one batch of expired candidate sets. The locked crawl row
    /// serializes cleanup with page commits and new generations. Keep the
    /// status row so expiry cannot reset retry backoff or cursor fencing.
    pub async fn prune_expired_space_browse(&self) -> Result<u64, StoreError> {
        let result = sqlx_core::query::query(
            "WITH expired AS MATERIALIZED (
                 SELECT account_id, space_id FROM space_browse_crawl
                 WHERE rooms_cached > 0
                   AND started_at <= floor(extract(epoch FROM statement_timestamp()) * 1000)::bigint - $1
                 ORDER BY started_at, account_id, space_id
                 LIMIT $2 FOR UPDATE SKIP LOCKED
             ), cleared AS (
                 DELETE FROM space_browse_rooms b USING expired e
                 WHERE b.account_id = e.account_id AND b.space_id = e.space_id
             )
             UPDATE space_browse_crawl c SET rooms_cached = 0 FROM expired e
             WHERE c.account_id = e.account_id AND c.space_id = e.space_id",
        )
        .bind(SPACE_BROWSE_TTL.as_millis() as i64)
        .bind(EXPIRED_ROOTS_PER_SWEEP)
        .execute(&self.maintenance_pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub(crate) async fn record_space_browse(
        tx: &mut Transaction<'_, Postgres>,
        account_id: Uuid,
        space_id: &str,
        now: i64,
        entries: &[DiscoveredEntry],
    ) -> Result<usize, StoreError> {
        let ids: Vec<&str> = entries.iter().map(|entry| entry.room_id.as_str()).collect();
        let summaries: Vec<&Value> = entries.iter().map(|entry| &entry.summary).collect();
        let withheld: Vec<Value> = entries
            .iter()
            .map(|entry| Value::from(entry.withheld.clone()))
            .collect();
        let children: Vec<Value> = entries
            .iter()
            .map(|entry| Value::Object(entry.children.clone()))
            .collect();
        let sql = format!(
            "WITH input AS (
                 SELECT DISTINCT ON (i.id) i.id, i.summary, i.withheld, i.children
                 FROM unnest($3::text[], $4::jsonb[], $5::jsonb[], $6::jsonb[]) WITH ORDINALITY
                   AS i(id, summary, withheld, children, ordinal)
                 WHERE i.id <> $2 ORDER BY i.id, i.ordinal
             )
             INSERT INTO space_browse_rooms (account_id, space_id, room_id, summary, withheld, children,
                                            observed_at, membership_epoch)
             SELECT $1, $2, i.id,
                    CASE WHEN NOT membership.joined THEN i.summary END,
                    CASE WHEN membership.joined THEN '[]'::jsonb ELSE i.withheld END,
                    CASE WHEN membership.joined THEN '{{}}'::jsonb ELSE i.children END, $7, s.membership_epoch
             FROM input i
             LEFT JOIN room_summaries s ON s.account_id = $1 AND s.room_id = i.id
             CROSS JOIN LATERAL (SELECT {joined} AS joined) membership
             ON CONFLICT (account_id, space_id, room_id) DO NOTHING",
            joined = joined_child("i.id"),
        );
        let result = sqlx_core::query::query(&sql)
            .bind(account_id)
            .bind(space_id)
            .bind(ids)
            .bind(summaries)
            .bind(withheld)
            .bind(children)
            .bind(now)
            .execute(&mut **tx)
            .await?;
        Ok(result.rows_affected() as usize)
    }

    /// All fresh, reachable links under one joined root, paged by edge ID.
    /// Joining a candidate changes its outgoing edges to synced state.
    pub async fn space_browse_links(
        &self,
        account_id: Uuid,
        space_id: &str,
        after: Option<(&str, &str)>,
        limit: i64,
    ) -> Result<Vec<SpaceBrowseLink>, StoreError> {
        let sql = format!(
            "{roots}
             SELECT link.parent_id, link.child_id, link.joined, link.sort_order,
                    link.origin_ts, link.suggested, link.discovered
             FROM roots root CROSS JOIN LATERAL (
                 {walk}
                 SELECT e.parent_id, e.child_id, child.joined, e.sort_order, e.origin_ts, e.suggested,
                        CASE WHEN NOT child.joined THEN jsonb_build_object(
                            'status', 'available', 'source', 'hierarchy', 'summary', child.summary,
                            'withheld', child.withheld, 'observed_at', child.observed_at,
                            'expires_at', root.fresh_until) END AS discovered
                 FROM edges e JOIN nodes child ON child.room_id = e.child_id
                 WHERE (e.parent_id, e.child_id) > ($3, $4)
                   AND EXISTS (SELECT 1 FROM walk WHERE room_id = e.parent_id AND depth < {SPACE_BROWSE_MAX_DEPTH})
                 ORDER BY e.parent_id, e.child_id LIMIT $5
             ) link
             ORDER BY link.parent_id, link.child_id",
            roots = roots("c.space_id = $2"),
            walk = walk(),
        );
        let (parent, child) = after.unwrap_or_default();
        let links = sqlx_core::query::query::<Postgres>(&sql)
            .bind(account_id)
            .bind(space_id)
            .bind(parent)
            .bind(child)
            .bind(limit.clamp(1, SPACE_BROWSE_MAX_PAGE) + 1)
            .try_map(|row| {
                Ok(SpaceBrowseLink {
                    parent_room_id: row.try_get("parent_id")?,
                    child_room_id: row.try_get("child_id")?,
                    joined: row.try_get("joined")?,
                    order: row.try_get("sort_order")?,
                    origin_ts: row.try_get("origin_ts")?,
                    suggested: row.try_get("suggested")?,
                    discovered: DiscoveredRow::from_json(row.try_get("discovered")?),
                })
            })
            .fetch_all(&self.pool)
            .await?;
        Ok(links)
    }

    /// Joined descendants reached through an unjoined space, for the sidebar.
    /// Projects freshly from the cache and current synced state on every read.
    pub async fn discovered_space_memberships(
        &self,
        account_id: Uuid,
        after: Option<(&str, &str)>,
        limit: i64,
    ) -> Result<Vec<DiscoveredSpaceMembership>, StoreError> {
        let sql = format!(
            "{roots}
             SELECT root.space_id, membership.room_id, root.fresh_until
             FROM roots root CROSS JOIN LATERAL (
                 {walk}
                 SELECT DISTINCT w.room_id
                 FROM walk w JOIN nodes n USING (room_id)
                 WHERE w.through_unjoined AND n.joined AND w.room_id <> root.space_id
                   AND (root.space_id, w.room_id) > ($2, $3)
                 ORDER BY w.room_id LIMIT $4
             ) membership
             ORDER BY root.space_id, membership.room_id LIMIT $4",
            roots = roots("c.space_id >= $2"),
            walk = walk(),
        );
        let (space, room) = after.unwrap_or_default();
        let rows: Vec<(String, String, i64)> = sqlx_core::query_as::query_as(&sql)
            .bind(account_id)
            .bind(space)
            .bind(room)
            .bind(limit.clamp(1, SPACE_BROWSE_MAX_PAGE) + 1)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(
                |(space_id, room_id, expires_at)| DiscoveredSpaceMembership {
                    space_id,
                    room_id,
                    expires_at,
                },
            )
            .collect())
    }
}

// Restrict the walk before touching candidates. PostgreSQL statement_timestamp()
// keeps freshness consistent across the entire query.
fn roots(filter: &str) -> String {
    let now = "floor(extract(epoch FROM statement_timestamp()) * 1000)::bigint";
    let ttl_ms = SPACE_BROWSE_TTL.as_millis();
    format!(
        "WITH roots AS (
             SELECT c.space_id, c.started_at + {ttl_ms} AS fresh_until
             FROM space_browse_crawl c
             JOIN room_summaries s ON s.account_id = c.account_id AND s.room_id = c.space_id
             JOIN accounts ac ON ac.account_id = c.account_id
             WHERE c.account_id = $1 AND ac.state = 'active' AND {filter}
               AND c.started_at > {now} - {ttl_ms} AND c.rooms_cached > 0
               AND {eligible} AND {joined}
               AND NOT EXISTS (SELECT 1 FROM room_upstream_reconcile r
                   WHERE r.account_id = $1 AND r.room_id = c.space_id AND r.state = 'gone')
         )",
        eligible = browse_root(),
        joined = visible_joined_room("c.space_id"),
    )
}

// Every target must have been returned in this root's crawl. A link starts
// from synced child state for a joined parent, or the validated children_state
// of a returned, unjoined space. The root itself uses synced state.
fn walk() -> String {
    format!(
        "WITH RECURSIVE nodes AS MATERIALIZED (
             SELECT b.room_id, b.summary, b.withheld, b.children, b.observed_at,
                    COALESCE({visible}, false) AS joined
             FROM space_browse_rooms b
             LEFT JOIN room_summaries summary ON summary.account_id = $1 AND summary.room_id = b.room_id
             WHERE b.account_id = $1 AND b.space_id = root.space_id
               AND CASE WHEN {joined} THEN {visible}
                        ELSE b.summary IS NOT NULL
                            AND b.membership_epoch IS NOT DISTINCT FROM summary.membership_epoch END
         ), parents AS MATERIALIZED (
             SELECT root.space_id AS room_id, true AS joined, '{{}}'::jsonb AS children
             UNION ALL
             SELECT n.room_id, n.joined, n.children FROM nodes n
             WHERE CASE WHEN n.joined THEN {synced_space}
                   ELSE n.summary->>'room_type' = 'm.space' END
         ), edges AS MATERIALIZED (
             SELECT p.room_id AS parent_id, n.room_id AS child_id, p.joined AS parent_joined,
                    {NORMALIZED_ORDER} AS sort_order, sc.origin_ts,
                    COALESCE(sc.content->'suggested' = 'true'::jsonb, false) AS suggested
             FROM parents p JOIN room_state sc ON sc.account_id = $1 AND sc.room_id = p.room_id
               AND sc.event_type = 'm.space.child' AND {SPACE_LINK_IS_CURRENT}
             JOIN nodes n ON n.room_id = sc.state_key WHERE p.joined
             UNION ALL
             SELECT p.room_id, n.room_id, false, link.value->>'order',
                    (link.value->>'origin_ts')::bigint, (link.value->>'suggested')::boolean
             FROM parents p CROSS JOIN LATERAL jsonb_each(p.children) link
             JOIN nodes n ON n.room_id = link.key WHERE NOT p.joined
         ), walk(room_id, depth, through_unjoined) AS (
             SELECT root.space_id, 0, false
             UNION
             SELECT e.child_id, w.depth + 1, w.through_unjoined OR NOT e.parent_joined
             FROM walk w JOIN edges e ON e.parent_id = w.room_id
             WHERE w.depth < {SPACE_BROWSE_MAX_DEPTH}
         )",
        joined = joined_child("b.room_id"),
        visible = visible_joined_room("b.room_id"),
        synced_space = synced_space("n.room_id"),
    )
}
