//! Space-hierarchy reads: `m.space.child` / `m.space.parent` resolved and
//! enriched with the referenced room's own cached display fields (issue #404,
//! ADR 0084).
//!
//! Axon already stores these state tuples like any other `room_state` row
//! (ADR 0016); nothing resolved them into a hierarchy before this. Unlike the
//! generic [`Store::room_state_of_type`](crate::Store::room_state_of_type),
//! space children need MSC1772 ordering (`order` string, then `origin_ts`,
//! then `room_id`) rather than the plain `state_key` order, so they get their
//! own queries here instead of reusing the generic read.

use serde_json::Value;
use sqlx_core::row::Row;
use sqlx_postgres::{PgRow, Postgres};
use uuid::Uuid;

use crate::space_discovery::{discovered_json, DiscoveredRow};
use crate::{Store, StoreError};

/// Hard cap on the number of rows [`Store::space_children`] and
/// [`Store::space_parents`] return. These are unpaginated single-shot reads,
/// but a remote room can hold an arbitrary number of distinct
/// `m.space.child`/`m.space.parent` state keys (bounded only by upstream
/// federation, not by anything Axon controls) — without a cap, one request
/// would drive unbounded database work, allocation, and response size (three
/// correlated enrichment lookups per row). Mirrors the same
/// hostile/pathological-room guard as `RELATION_READ_CAP` in `events.rs`;
/// children are truncated *after* the MSC1772 sort, so the cap always keeps
/// the leading (highest-priority) entries rather than an arbitrary subset.
const SPACE_HIERARCHY_CAP: i64 = 1000;

/// SQL predicate, over unqualified `room_state` columns, that a
/// `m.space.child`/`m.space.parent` tuple is a current link: not redacted, and
/// carrying a non-empty `via` array of non-empty strings. The Matrix spec
/// ignores space links without a valid `via`; that is also how a link is
/// removed (the tuple is replaced with empty content). Shared by the paged
/// hierarchy read below and the image-pack ancestry walk so the two never
/// disagree about which links exist.
pub(crate) const SPACE_LINK_IS_CURRENT: &str = r#"redacted IS DISTINCT FROM true
            AND jsonb_typeof(content->'via')='array'
            AND content->'via'<>'[]'::jsonb
            AND NOT jsonb_path_exists(content->'via', '$[*] ? (@.type() != "string" || @ == "")')"#;

/// Normalized MSC1772 `order` of an `m.space.child` tuple aliased `sc`: the
/// `content.order` string when it is at most 50 characters of printable ASCII
/// (`0x20`–`0x7E`), and `NULL` otherwise. The spec treats a non-string or
/// out-of-range `order` as absent, so it must sort with the absent group,
/// not wherever its JSON text would land.
const NORMALIZED_ORDER: &str = r#"CASE WHEN jsonb_typeof(sc.content->'order')='string'
                    AND (sc.content->>'order') ~ '^[\x20-\x7E]{0,50}$'
               THEN sc.content->>'order' END"#;

/// The Matrix spec's limit on a room id. A child "room id" longer than this
/// is not a room any homeserver can serve, and the paged read leaves it out,
/// so every row's id fits in a bounded cursor.
const MAX_ROOM_ID_BYTES: usize = 255;

/// Largest page [`Store::space_hierarchy_page`] serves. The API clamps its
/// `limit` to this too; the store repeats the bound so no caller can turn
/// one read into an unbounded scan.
pub const SPACE_HIERARCHY_MAX_PAGE: i64 = 500;

/// Pull `content.via` (a list of server names) out of a state event's content,
/// defaulting to empty when absent or malformed. Shared by both row types
/// below since `m.space.child` and `m.space.parent` carry the same `via`
/// field.
fn via_from_content(content: &Option<Value>) -> Vec<String> {
    content
        .as_ref()
        .and_then(|c| c.get("via"))
        .and_then(|v| v.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Pull `content.suggested` out of an `m.space.child` content, `false` when
/// absent or not a boolean.
fn suggested_from_content(content: &Option<Value>) -> bool {
    content
        .as_ref()
        .and_then(|c| c.get("suggested"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// One `m.space.child` tuple (a room `room_id` is spaced into), enriched with
/// the child room's own cached display fields — `None` when Axon doesn't
/// know that room (e.g. the account was never joined to it).
#[derive(Debug, Clone)]
pub struct SpaceChildRow {
    /// The child room's id (the `m.space.child` state key).
    pub room_id: String,
    /// Federation-resolution hints (`content.via`) for a child Axon has no
    /// direct path to.
    pub via: Vec<String>,
    /// MSC1772 sort key, if set. Absent sorts last (see [`Store::space_children`]).
    pub order: Option<String>,
    /// Whether the child is flagged as suggested (`content.suggested`,
    /// defaults to `false` when absent).
    pub suggested: bool,
    /// The child room's cached `m.room.name`, if known.
    pub name: Option<String>,
    /// The child room's cached `m.room.avatar` `url`, if known.
    pub avatar_url: Option<String>,
    /// The child room's `m.room.create` `type` (e.g. `m.space`), if known.
    pub room_type: Option<String>,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for SpaceChildRow {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        let content: Option<Value> = row.try_get("content")?;
        let order = content
            .as_ref()
            .and_then(|c| c.get("order"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        Ok(SpaceChildRow {
            room_id: row.try_get("room_id")?,
            via: via_from_content(&content),
            order,
            suggested: suggested_from_content(&content),
            name: row.try_get("name")?,
            avatar_url: row.try_get("avatar_url")?,
            room_type: row.try_get("room_type")?,
        })
    }
}

/// One `m.space.parent` tuple (a space `room_id` claims to belong to),
/// enriched the same way as [`SpaceChildRow`].
#[derive(Debug, Clone)]
pub struct SpaceParentRow {
    /// The parent space's room id (the `m.space.parent` state key).
    pub room_id: String,
    /// Federation-resolution hints (`content.via`).
    pub via: Vec<String>,
    /// Whether this parent is the canonical one (`content.canonical`,
    /// defaults to `false` when absent).
    pub canonical: bool,
    /// The parent's cached `m.room.name`, if known.
    pub name: Option<String>,
    /// The parent's cached `m.room.avatar` `url`, if known.
    pub avatar_url: Option<String>,
    /// The parent's `m.room.create` `type` (normally `m.space`), if known.
    pub room_type: Option<String>,
}

impl sqlx_core::from_row::FromRow<'_, PgRow> for SpaceParentRow {
    fn from_row(row: &PgRow) -> Result<Self, sqlx_core::Error> {
        let content: Option<Value> = row.try_get("content")?;
        let canonical = content
            .as_ref()
            .and_then(|c| c.get("canonical"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        Ok(SpaceParentRow {
            room_id: row.try_get("room_id")?,
            via: via_from_content(&content),
            canonical,
            name: row.try_get("name")?,
            avatar_url: row.try_get("avatar_url")?,
            room_type: row.try_get("room_type")?,
        })
    }
}

/// Keyset position in the paged hierarchy read: the last row a page
/// returned, by its MSC1772 sort key. `order` is the normalized order
/// (`None` for the absent-last group), so the cursor knows which group the
/// row sorted in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceHierarchyCursor {
    /// Normalized `order` of the last row, `None` when it sorted as absent.
    pub order: Option<String>,
    /// `origin_ts` of the last row's `m.space.child` event.
    pub origin_ts: i64,
    /// The last row's child room id.
    pub room_id: String,
}

/// One row of the paged hierarchy read: a current `m.space.child` link with
/// the child's cached display fields and the account's membership in it.
#[derive(Debug, Clone)]
pub struct SpaceHierarchyRow {
    /// The link and the child's cached display fields. `order` here is the
    /// normalized order the row was sorted by.
    pub child: SpaceChildRow,
    /// `origin_ts` of the `m.space.child` event, part of the sort key.
    pub origin_ts: i64,
    /// Whether the account's own `m.room.member` in the child is `join`.
    pub joined: bool,
    /// The child's discovery row (ADR 0128 decision 4). Always `None` for a
    /// joined child: synced state is authoritative for joined rooms.
    pub discovered: Option<DiscoveredRow>,
}

impl SpaceHierarchyRow {
    /// The keyset position just after this row.
    pub fn cursor(&self) -> SpaceHierarchyCursor {
        SpaceHierarchyCursor {
            order: self.child.order.clone(),
            origin_ts: self.origin_ts,
            room_id: self.child.room_id.clone(),
        }
    }
}

/// One page of [`Store::space_hierarchy_page`].
#[derive(Debug, Clone)]
pub struct SpaceHierarchyPage {
    /// Up to `limit` rows in MSC1772 order, strictly after the request cursor.
    pub rows: Vec<SpaceHierarchyRow>,
    /// Whether at least one more link sorts after the last row.
    pub has_more: bool,
    /// The space's current child links, counted in the same snapshot.
    pub total_children: i64,
}

impl Store {
    /// List `room_id`'s space children (`m.space.child`), MSC1772-ordered:
    /// `order` string ascending (a child with no `order` sorts after every
    /// child that has one), then `origin_ts`, then child `room_id` as a
    /// final tiebreak. Each child is enriched with its own cached
    /// name/avatar/room_type via a correlated point lookup into `room_state`
    /// keyed on the child's `room_id` — `None` when Axon doesn't know that
    /// room. An unknown or childless `room_id` yields an empty list. Capped at
    /// [`SPACE_HIERARCHY_CAP`] rows (applied after the MSC1772 sort, so a
    /// truncated result still keeps the leading/highest-priority children).
    pub async fn space_children(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<Vec<SpaceChildRow>, StoreError> {
        let sql = format!(
            "SELECT sc.state_key AS room_id, sc.content, \
                    (SELECT rs.content->>'name' FROM room_state rs \
                       WHERE rs.account_id = sc.account_id AND rs.room_id = sc.state_key \
                         AND rs.event_type = 'm.room.name' AND rs.state_key = '') AS name, \
                    (SELECT rs.content->>'url' FROM room_state rs \
                       WHERE rs.account_id = sc.account_id AND rs.room_id = sc.state_key \
                         AND rs.event_type = 'm.room.avatar' AND rs.state_key = '') AS avatar_url, \
                    (SELECT rs.content->>'type' FROM room_state rs \
                       WHERE rs.account_id = sc.account_id AND rs.room_id = sc.state_key \
                         AND rs.event_type = 'm.room.create' AND rs.state_key = '') AS room_type \
             FROM room_state sc \
             WHERE sc.account_id = $1 AND sc.room_id = $2 AND sc.event_type = 'm.space.child' \
             ORDER BY (sc.content->>'order') ASC NULLS LAST, sc.origin_ts ASC, sc.state_key ASC \
             LIMIT {SPACE_HIERARCHY_CAP}"
        );
        let rows = sqlx_core::query_as::query_as::<Postgres, SpaceChildRow>(&sql)
            .bind(account_id)
            .bind(room_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// List the spaces `room_id` claims as a parent (`m.space.parent`) — the
    /// reverse lookup of [`space_children`](Self::space_children). No
    /// ordering is spec-defined here (unlike children's `order` field), so
    /// results are ordered by parent `room_id` for determinism. Same
    /// per-parent enrichment as children, and the same [`SPACE_HIERARCHY_CAP`]
    /// row cap.
    pub async fn space_parents(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<Vec<SpaceParentRow>, StoreError> {
        let sql = format!(
            "SELECT sp.state_key AS room_id, sp.content, \
                    (SELECT rs.content->>'name' FROM room_state rs \
                       WHERE rs.account_id = sp.account_id AND rs.room_id = sp.state_key \
                         AND rs.event_type = 'm.room.name' AND rs.state_key = '') AS name, \
                    (SELECT rs.content->>'url' FROM room_state rs \
                       WHERE rs.account_id = sp.account_id AND rs.room_id = sp.state_key \
                         AND rs.event_type = 'm.room.avatar' AND rs.state_key = '') AS avatar_url, \
                    (SELECT rs.content->>'type' FROM room_state rs \
                       WHERE rs.account_id = sp.account_id AND rs.room_id = sp.state_key \
                         AND rs.event_type = 'm.room.create' AND rs.state_key = '') AS room_type \
             FROM room_state sp \
             WHERE sp.account_id = $1 AND sp.room_id = $2 AND sp.event_type = 'm.space.parent' \
             ORDER BY sp.state_key ASC \
             LIMIT {SPACE_HIERARCHY_CAP}"
        );
        let rows = sqlx_core::query_as::query_as::<Postgres, SpaceParentRow>(&sql)
            .bind(account_id)
            .bind(room_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// One page of `space_room_id`'s current child links (ADR 0128 decision
    /// 1), in MSC1772 order: normalized `order` ascending by code point with
    /// absent last, then `origin_ts`, then child room id by code point. Only
    /// links with a valid `via` count ([`SPACE_LINK_IS_CURRENT`]), both in the
    /// rows and in `total_children`, and only child ids within the spec's
    /// 255-byte room-id limit. Rows start strictly after `after`, so a
    /// link added or removed between pages neither duplicates nor skips any
    /// other link. Reads only local state; an unknown account or space is an
    /// empty page. `limit` is clamped to `1..=`[`SPACE_HIERARCHY_MAX_PAGE`].
    ///
    /// The page, its look-ahead row, and the total come from one statement,
    /// so they share a snapshot, and a page past the end still reports the
    /// total. The whole-space scan keeps only each link's sort key; a link's
    /// `via` and `suggested`, and the child's display and membership lookups,
    /// are read only for the page's rows. Every per-row lookup, `joined`
    /// included, is a primary-key point lookup: the account's user id is an
    /// uncorrelated subquery, so the member check never walks a child's
    /// membership list.
    pub async fn space_hierarchy_page(
        &self,
        account_id: Uuid,
        space_room_id: &str,
        after: Option<&SpaceHierarchyCursor>,
        limit: i64,
    ) -> Result<SpaceHierarchyPage, StoreError> {
        let limit = limit.clamp(1, SPACE_HIERARCHY_MAX_PAGE);
        // `(order IS NULL, COALESCE(order, ''))` puts the absent group last
        // and lets one row comparison page across the group boundary; the
        // empty string never collides because the flag differs.
        let sort_key = r#"(sort_order IS NULL), COALESCE(sort_order, '') COLLATE "C",
                 origin_ts, child_id COLLATE "C""#;
        let sql = format!(
            r#"WITH links AS MATERIALIZED (
                 SELECT sc.state_key AS child_id, sc.origin_ts,
                        {NORMALIZED_ORDER} AS sort_order
                 FROM room_state sc
                 WHERE sc.account_id = $1 AND sc.room_id = $2
                   AND sc.event_type = 'm.space.child'
                   AND octet_length(sc.state_key) <= {MAX_ROOM_ID_BYTES}
                   AND {SPACE_LINK_IS_CURRENT}
               ), page AS (
                 SELECT * FROM links
                 WHERE NOT $3::boolean
                    OR ({sort_key}) > ($4::boolean, $5::text COLLATE "C", $6::bigint, $7::text COLLATE "C")
                 ORDER BY {sort_key}
                 LIMIT $8
               )
               SELECT t.total_children, p.child_id AS room_id, p.sort_order, p.origin_ts,
                      (SELECT jsonb_build_object('via', sc.content->'via',
                                                 'suggested', sc.content->'suggested')
                         FROM room_state sc
                         WHERE sc.account_id = $1 AND sc.room_id = $2
                           AND sc.event_type = 'm.space.child' AND sc.state_key = p.child_id) AS content,
                      (SELECT rs.content->>'name' FROM room_state rs
                         WHERE rs.account_id = $1 AND rs.room_id = p.child_id
                           AND rs.event_type = 'm.room.name' AND rs.state_key = '') AS name,
                      (SELECT rs.content->>'url' FROM room_state rs
                         WHERE rs.account_id = $1 AND rs.room_id = p.child_id
                           AND rs.event_type = 'm.room.avatar' AND rs.state_key = '') AS avatar_url,
                      (SELECT rs.content->>'type' FROM room_state rs
                         WHERE rs.account_id = $1 AND rs.room_id = p.child_id
                           AND rs.event_type = 'm.room.create' AND rs.state_key = '') AS room_type,
                      EXISTS (SELECT 1 FROM room_state m
                         WHERE m.account_id = $1 AND m.room_id = p.child_id
                           AND m.event_type = 'm.room.member'
                           AND m.state_key = (SELECT ac.user_id FROM accounts ac WHERE ac.account_id = $1)
                           AND m.content->>'membership' = 'join'
                           AND m.redacted IS DISTINCT FROM true) AS joined,
                      {discovered} AS discovered
               FROM (SELECT count(*) AS total_children FROM links) t
               LEFT JOIN page p ON true
               ORDER BY {sort_key}"#,
            discovered = discovered_json("p.child_id"),
        );
        // The look-ahead row decides `has_more`, so the last page is the one
        // that comes back short, even when the space divides into full pages.
        let rows = sqlx_core::query::query::<Postgres>(&sql)
            .bind(account_id)
            .bind(space_room_id)
            .bind(after.is_some())
            .bind(after.map(|c| c.order.is_none()).unwrap_or(false))
            .bind(after.and_then(|c| c.order.as_deref()).unwrap_or(""))
            .bind(after.map(|c| c.origin_ts).unwrap_or(0))
            .bind(after.map(|c| c.room_id.as_str()).unwrap_or(""))
            .bind(limit + 1)
            .fetch_all(&self.pool)
            .await?;

        // The total rides on every row; an empty page is one row whose page
        // columns are all NULL, so even a page past the end carries it.
        let total_children = match rows.first() {
            Some(row) => row.try_get::<i64, _>("total_children")?,
            None => 0,
        };
        let mut page = Vec::with_capacity(rows.len().min(limit as usize));
        let mut has_more = false;
        for row in &rows {
            let Some(room_id) = row.try_get::<Option<String>, _>("room_id")? else {
                continue;
            };
            if page.len() as i64 == limit {
                has_more = true;
                break;
            }
            let content: Option<Value> = row.try_get("content")?;
            let joined: bool = row.try_get("joined")?;
            // A row a commit wrote while a join raced it is never shown for
            // the joined room; the child's later leave deletes it.
            let discovered = if joined {
                None
            } else {
                DiscoveredRow::from_json(row.try_get("discovered")?)
            };
            page.push(SpaceHierarchyRow {
                child: SpaceChildRow {
                    room_id,
                    via: via_from_content(&content),
                    order: row.try_get("sort_order")?,
                    suggested: suggested_from_content(&content),
                    name: row.try_get("name")?,
                    avatar_url: row.try_get("avatar_url")?,
                    room_type: row.try_get("room_type")?,
                },
                origin_ts: row.try_get("origin_ts")?,
                joined,
                discovered,
            });
        }
        Ok(SpaceHierarchyPage {
            rows: page,
            has_more,
            total_children,
        })
    }
}
