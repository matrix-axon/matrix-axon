//! Paged account-wide space links between rooms the account has joined.

use sqlx_postgres::Postgres;
use uuid::Uuid;

use crate::space_discovery::joined_child;
use crate::spaces::{
    MAX_ROOM_ID_BYTES, NORMALIZED_ORDER, SPACE_LINK_IS_CURRENT, SPACE_PARENT_POWER,
    SPACE_PARENT_VERIFIED,
};
use crate::{Store, StoreError};

pub const SPACE_GRAPH_MAX_PAGE: i64 = 1000;

/// The immutable identity of a state tuple within the account, ordered by
/// source room, event type, and state key. Reordering a child won't move it
/// across the page boundary.
#[derive(Debug)]
pub struct SpaceGraphKey {
    pub room_id: String,
    pub event_type: String,
    pub state_key: String,
}

#[derive(Debug)]
pub struct SpaceGraphLink {
    pub key: SpaceGraphKey,
    pub sort_order: Option<String>,
    pub origin_ts: i64,
    pub canonical: bool,
}

pub struct SpaceGraphPage {
    pub links: Vec<SpaceGraphLink>,
    pub has_more: bool,
}

impl Store {
    /// Read local child links and verified parent claims. Both endpoints of
    /// a link must be visible joined rooms, and the parent must be a space.
    /// This returns edges, including shared and cyclic edges, without walking
    /// them. Clients can combine the result with their existing room list.
    pub async fn space_graph_page(
        &self,
        account_id: Uuid,
        after: Option<&SpaceGraphKey>,
        limit: i64,
    ) -> Result<SpaceGraphPage, StoreError> {
        let limit = limit.clamp(1, SPACE_GRAPH_MAX_PAGE);
        let sql = format!(
            r#"WITH space_links AS NOT MATERIALIZED (
                SELECT * FROM room_state
                WHERE account_id = $1 AND event_type IN ('m.space.child', 'm.space.parent')
                  AND {SPACE_LINK_IS_CURRENT}
            )
            SELECT sc.room_id, sc.event_type, sc.state_key, sc.origin_ts,
                   CASE WHEN sc.event_type = 'm.space.child' THEN {NORMALIZED_ORDER} END AS sort_order,
                   COALESCE(sc.event_type = 'm.space.parent' AND sc.content->'canonical' = 'true'::jsonb,
                            false) AS canonical
            FROM space_links sc
            JOIN accounts account ON account.account_id = $1 AND account.state = 'active'
            JOIN room_summaries source ON source.account_id = $1 AND source.room_id = sc.room_id
                AND NOT source.hidden_left AND NOT source.hidden_tombstoned
            JOIN room_summaries target ON target.account_id = $1 AND target.room_id = sc.state_key
                AND NOT target.hidden_left AND NOT target.hidden_tombstoned
            JOIN room_state creation ON creation.account_id = $1
                AND creation.room_id = CASE WHEN sc.event_type = 'm.space.child'
                    THEN sc.room_id ELSE sc.state_key END
                AND creation.event_type = 'm.room.create' AND creation.state_key = ''
                AND creation.content->>'type' = 'm.space'
            LEFT JOIN space_links reciprocal ON sc.event_type = 'm.space.parent'
                AND reciprocal.room_id = sc.state_key AND reciprocal.event_type = 'm.space.child'
                AND reciprocal.state_key = sc.room_id
            LEFT JOIN room_state levels ON sc.event_type = 'm.space.parent'
                AND levels.account_id = $1 AND levels.room_id = sc.state_key
                AND levels.event_type = 'm.room.power_levels' AND levels.state_key = ''
            LEFT JOIN room_state member ON sc.event_type = 'm.space.parent'
                AND member.account_id = $1 AND member.room_id = sc.state_key
                AND member.event_type = 'm.room.member' AND member.state_key = sc.sender
                AND member.redacted IS DISTINCT FROM true
            {SPACE_PARENT_POWER}
            WHERE (sc.room_id, sc.event_type, sc.state_key) > ($2, $3, $4)
              AND octet_length(sc.room_id) <= {MAX_ROOM_ID_BYTES}
              AND octet_length(sc.state_key) <= {MAX_ROOM_ID_BYTES}
              AND {source_joined} AND {target_joined}
              AND (sc.event_type = 'm.space.child' OR {SPACE_PARENT_VERIFIED})
            ORDER BY sc.room_id, sc.event_type, sc.state_key
            LIMIT $5"#,
            source_joined = joined_child("sc.room_id"),
            target_joined = joined_child("sc.state_key"),
        );
        let rows = sqlx_core::query_as::query_as::<Postgres, _>(&sql)
            .bind(account_id)
            .bind(after.map_or("", |key| &key.room_id))
            .bind(after.map_or("", |key| &key.event_type))
            .bind(after.map_or("", |key| &key.state_key))
            .bind(limit + 1)
            .fetch_all(&self.pool)
            .await?;
        let has_more = rows.len() > limit as usize;
        let links = rows
            .into_iter()
            .take(limit as usize)
            .map(
                |(room_id, event_type, state_key, origin_ts, sort_order, canonical)| {
                    SpaceGraphLink {
                        key: SpaceGraphKey {
                            room_id,
                            event_type,
                            state_key,
                        },
                        sort_order,
                        origin_ts,
                        canonical,
                    }
                },
            )
            .collect();
        Ok(SpaceGraphPage { links, has_more })
    }
}
