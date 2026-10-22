//! Bounded image-pack snapshots and account-scoped preview authorization.

use axon_core::image_pack::{parse_image_pack, MAX_PACK_BYTES, MAX_ROOM_PACKS};
use serde_json::Value;
use sqlx_core::row::Row;
use uuid::Uuid;

use crate::{Store, StoreError};

#[derive(Debug)]
pub struct ImagePackRow {
    /// None denotes the account's personal pack.
    pub room_id: Option<String>,
    pub event_type: String,
    pub state_key: Option<String>,
    pub event_id: Option<String>,
    pub content: Option<Value>,
    pub oversized: bool,
}

/// Discovery metadata shares the same database snapshot as the pack rows.
#[derive(Debug)]
pub struct ImagePackSnapshot {
    pub rows: Vec<ImagePackRow>,
    /// A cycle or the ancestor depth limit prevented further traversal.
    pub partial: bool,
}

// One canonical chain, not the potentially exponential space graph. Sixteen
// ancestors is a discovery budget, independent of the shared pack/image caps.
const MAX_ANCESTORS: usize = 16;

// Resolve only locally verifiable parent claims. A reciprocal link or current
// permission in the parent is required by the Matrix space relationship rules.
// The lateral LIMIT selects the canonical parent deterministically before the
// recursive step; COLLATE C implements the spec's codepoint ordering.
const ANCESTORS: &str = r#",
    space_links AS NOT MATERIALIZED (
        SELECT * FROM room_state WHERE account_id=$1
            AND event_type IN ('m.space.parent','m.space.child')
            AND redacted IS DISTINCT FROM true
            AND jsonb_typeof(content->'via')='array'
            AND content->'via'<>'[]'::jsonb
            AND NOT jsonb_path_exists(content->'via', '$[*] ? (@.type() != "string" || @ == "")')
    ), ancestry(room_id, depth, visited, cycle) AS (
        SELECT $2::text, 0, ARRAY[$2::text], false
        UNION ALL
        SELECT parent.room_id, chain.depth + 1,
               chain.visited || parent.room_id, parent.room_id=ANY(chain.visited)
        FROM ancestry chain
        CROSS JOIN LATERAL (
            SELECT link.state_key AS room_id
            FROM space_links link
            JOIN joined_rooms joined ON joined.room_id=link.state_key
            JOIN room_state creation ON creation.account_id=$1
                AND creation.room_id=link.state_key
                AND creation.event_type='m.room.create' AND creation.state_key=''
                AND creation.content->>'type'='m.space'
            LEFT JOIN space_links reciprocal ON reciprocal.account_id=$1
                AND reciprocal.room_id=link.state_key
                AND reciprocal.event_type='m.space.child' AND reciprocal.state_key=chain.room_id
            LEFT JOIN room_state levels ON levels.account_id=$1
                AND levels.room_id=link.state_key
                AND levels.event_type='m.room.power_levels' AND levels.state_key=''
            LEFT JOIN room_state member ON member.account_id=$1
                AND member.room_id=link.state_key
                AND member.event_type='m.room.member' AND member.state_key=link.sender
                AND member.redacted IS DISTINCT FROM true
            CROSS JOIN LATERAL (
                SELECT COALESCE(levels.content#>ARRAY['users',link.sender],
                                levels.content->'users_default','0'::jsonb) AS sender_level,
                       COALESCE(levels.content#>'{events,m.space.child}',
                                levels.content->'state_default','50'::jsonb) AS required_level
            ) power
            WHERE link.account_id=$1 AND link.room_id=chain.room_id
                AND link.event_type='m.space.parent'
                AND link.content->'canonical'='true'::jsonb
                AND (
                    reciprocal.event_id IS NOT NULL
                    OR (member.content->>'membership'='join' AND (
                        -- Without power levels the creator has power 100.
                        (levels.event_id IS NULL AND
                         COALESCE(creation.content->>'creator',creation.sender)=link.sender)
                        -- Room v12 creators have unbounded power.
                        OR (creation.content->>'room_version'='12' AND
                            (creation.sender=link.sender OR creation.content->'additional_creators' ? link.sender))
                        OR (levels.event_id IS NOT NULL AND
                            CASE WHEN power.sender_level::text ~ '^-?[0-9]{1,16}$'
                                   AND power.required_level::text ~ '^-?[0-9]{1,16}$'
                                 THEN abs(power.sender_level::text::numeric)<=9007199254740991
                                  AND abs(power.required_level::text::numeric)<=9007199254740991
                                  AND power.sender_level::text::numeric>=power.required_level::text::numeric
                                 ELSE false END)
                    ))
                )
            ORDER BY link.state_key COLLATE "C" LIMIT 1
        ) parent
        WHERE chain.depth < {max_depth} AND NOT chain.cycle
    ), hierarchy AS (
        SELECT room_id,depth FROM ancestry WHERE depth<={max_ancestors} AND NOT cycle
    ), hierarchy_status AS (
        SELECT EXISTS(SELECT 1 FROM ancestry WHERE depth={max_depth} OR cycle) AS partial
    )
"#;

// Current room packs require positive membership. Stable tuples supersede
// legacy tuples with the same state key, including when the stable tuple was
// cleared/redacted. Personal account data never crosses account boundaries.
const SOURCES: &str = "WITH RECURSIVE joined_rooms AS MATERIALIZED ( \
    SELECT membership.room_id FROM room_state membership JOIN accounts ac \
      ON ac.account_id=membership.account_id \
    WHERE membership.account_id=$1 AND ac.state='active' \
      AND membership.event_type='m.room.member' AND membership.state_key=ac.user_id \
      AND membership.content->>'membership'='join' AND membership.redacted IS DISTINCT FROM true \
), sources AS ( \
    SELECT rs.room_id,rs.event_type,rs.state_key,rs.event_id,rs.content \
    FROM room_state rs JOIN joined_rooms joined ON joined.room_id=rs.room_id \
    WHERE rs.account_id=$1 \
      AND rs.event_type IN ('m.room.image_pack','im.ponies.room_emotes') \
      AND rs.redacted IS DISTINCT FROM true \
      AND (rs.event_type='m.room.image_pack' OR NOT EXISTS ( \
        SELECT 1 FROM room_state stable WHERE stable.account_id=rs.account_id \
        AND stable.room_id=rs.room_id AND stable.event_type='m.room.image_pack' \
        AND stable.state_key=rs.state_key)) \
    UNION ALL \
    SELECT NULL::text,ad.event_type,NULL::text,NULL::text,ad.content \
    FROM account_data ad JOIN accounts ac ON ac.account_id=ad.account_id \
    WHERE ad.account_id=$1 AND ac.state='active' AND ad.room_id='' \
      AND ad.event_type='im.ponies.user_emotes' \
) ";

impl Store {
    /// Joined-room check and pack discovery share one statement snapshot.
    pub async fn image_pack_snapshot(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<Option<ImagePackSnapshot>, StoreError> {
        let (joined, rows, partial) = self
            .image_pack_source_page(account_id, Some(room_id), None, None)
            .await?;
        Ok(joined.then_some(ImagePackSnapshot { rows, partial }))
    }

    // Keep each transfer bounded, but allow authorization to continue past
    // sticker-only or invalid matching packs. A cursor avoids OFFSET rescans.
    async fn image_pack_source_page(
        &self,
        account_id: Uuid,
        room_id: Option<&str>,
        mxc: Option<&str>,
        after: Option<&(String, String, String)>,
    ) -> Result<(bool, Vec<ImagePackRow>, bool), StoreError> {
        let hierarchy = if room_id.is_some() {
            ANCESTORS
                .replace("{max_depth}", &(MAX_ANCESTORS + 1).to_string())
                .replace("{max_ancestors}", &MAX_ANCESTORS.to_string())
        } else {
            String::new()
        };
        let scope = if room_id.is_some() {
            "room_id IS NULL OR room_id IN (SELECT room_id FROM hierarchy)"
        } else {
            "true"
        };
        let rank = if room_id.is_some() {
            "COALESCE((SELECT depth+1 FROM hierarchy h WHERE h.room_id=bounded.room_id),0)"
        } else {
            "0"
        };
        let partial = if room_id.is_some() {
            "(SELECT partial FROM hierarchy_status)"
        } else {
            "false"
        };
        let sql = format!(
            "{SOURCES}{hierarchy}, authorized AS (SELECT $2::text IS NULL OR EXISTS ( \
             SELECT 1 FROM joined_rooms WHERE room_id=$2) AS joined), \
             candidates AS MATERIALIZED (SELECT * FROM sources \
             WHERE ({scope}) \
             AND ($4::text IS NULL OR (COALESCE(room_id,''),event_type,COALESCE(state_key,'')) > ($4,$5,$6)) \
             AND ($3::text IS NULL OR content#>>'{{pack,avatar_url}}'=$3 OR \
             jsonb_path_exists(content,'$.images.*.url ? (@ == $mxc)',jsonb_build_object('mxc',$3::text)))), \
             measured AS MATERIALIZED (SELECT *, CASE WHEN pg_column_size(content) > {MAX_PACK_BYTES} \
             THEN true ELSE COALESCE(octet_length(content::text) > {MAX_PACK_BYTES},false) END AS oversized \
             FROM candidates), \
             bounded AS MATERIALIZED (SELECT room_id,event_type,state_key,event_id, \
             CASE WHEN NOT oversized THEN content END AS content,oversized FROM measured), \
             selected AS (SELECT *, {rank} AS source_rank FROM bounded \
             ORDER BY source_rank,COALESCE(room_id,''),event_type,COALESCE(state_key,'') LIMIT $7) \
             SELECT authorized.joined,{partial} AS hierarchy_partial,selected.* FROM authorized LEFT JOIN selected ON authorized.joined \
             ORDER BY source_rank,COALESCE(room_id,''),event_type,COALESCE(state_key,'')"
        );
        let rows = sqlx_core::query::query(&sql)
            .bind(account_id)
            .bind(room_id)
            .bind(mxc)
            .bind(after.map(|v| v.0.as_str()))
            .bind(after.map(|v| v.1.as_str()))
            .bind(after.map(|v| v.2.as_str()))
            .bind((MAX_ROOM_PACKS + 2) as i64)
            .fetch_all(&self.read_pool)
            .await?;
        let mut joined = false;
        let mut sources = Vec::new();
        let mut partial = false;
        for r in rows {
            joined = r.try_get("joined")?;
            partial = r.try_get("hierarchy_partial")?;
            let Some(event_type) = r.try_get::<Option<String>, _>("event_type")? else {
                continue;
            };
            sources.push(ImagePackRow {
                room_id: r.try_get("room_id")?,
                event_type,
                state_key: r.try_get("state_key")?,
                event_id: r.try_get("event_id")?,
                content: r.try_get("content")?,
                oversized: r.try_get("oversized")?,
            });
        }
        Ok((joined, sources, partial))
    }

    /// Scan bounded pages until a parsed image/avatar matches. Exhaustion is
    /// the only negative answer: a deadline is an operational error, not 404.
    pub async fn image_pack_authorizes_media(
        &self,
        account_id: Uuid,
        mxc: &str,
    ) -> Result<bool, StoreError> {
        if axon_core::media::parse_mxc(mxc).is_none() {
            return Ok(false);
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut after = None;
            loop {
                let (_, rows, _) = self
                    .image_pack_source_page(account_id, None, Some(mxc), after.as_ref())
                    .await?;
                let exhausted = rows.len() < MAX_ROOM_PACKS + 2;
                for row in rows {
                    after = Some((
                        row.room_id.unwrap_or_default(),
                        row.event_type,
                        row.state_key.unwrap_or_default(),
                    ));
                    if row
                        .content
                        .and_then(|c| parse_image_pack(&c))
                        .is_some_and(|pack| {
                            pack.avatar_url.as_deref() == Some(mxc)
                                || pack.images.iter().any(|image| image.url == mxc)
                        })
                    {
                        return Ok(true);
                    }
                }
                if exhausted {
                    return Ok(false);
                }
            }
        })
        .await
        .map_err(|_| {
            StoreError::Sqlx(sqlx_core::Error::Protocol(
                "image pack authorization deadline exceeded".into(),
            ))
        })?
    }
}
