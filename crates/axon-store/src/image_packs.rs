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

// Current room packs require positive membership. Stable tuples supersede
// legacy tuples with the same state key, including when the stable tuple was
// cleared/redacted. Personal account data never crosses account boundaries.
const SOURCES: &str = "WITH joined_rooms AS MATERIALIZED ( \
    SELECT membership.room_id FROM room_state membership JOIN accounts ac \
      ON ac.account_id=membership.account_id \
    WHERE membership.account_id=$1 AND ac.state='active' \
      AND membership.event_type='m.room.member' AND membership.state_key=ac.user_id \
      AND membership.content->>'membership'='join' \
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
    ) -> Result<Option<Vec<ImagePackRow>>, StoreError> {
        let (joined, rows) = self
            .image_pack_source_page(account_id, Some(room_id), None, None)
            .await?;
        Ok(joined.then_some(rows))
    }

    // Keep each transfer bounded, but allow authorization to continue past
    // sticker-only or invalid matching packs. A cursor avoids OFFSET rescans.
    async fn image_pack_source_page(
        &self,
        account_id: Uuid,
        room_id: Option<&str>,
        mxc: Option<&str>,
        after: Option<&(String, String, String)>,
    ) -> Result<(bool, Vec<ImagePackRow>), StoreError> {
        let sql = format!(
            "{SOURCES}, authorized AS (SELECT $2::text IS NULL OR EXISTS ( \
             SELECT 1 FROM joined_rooms WHERE room_id=$2) AS joined), \
             candidates AS MATERIALIZED (SELECT * FROM sources \
             WHERE ($2::text IS NULL OR room_id=$2 OR room_id IS NULL) \
             AND ($4::text IS NULL OR (COALESCE(room_id,''),event_type,COALESCE(state_key,'')) > ($4,$5,$6)) \
             AND ($3::text IS NULL OR content#>>'{{pack,avatar_url}}'=$3 OR \
             jsonb_path_exists(content,'$.images.*.url ? (@ == $mxc)',jsonb_build_object('mxc',$3::text)))), \
             measured AS MATERIALIZED (SELECT *, CASE WHEN pg_column_size(content) > {MAX_PACK_BYTES} \
             THEN true ELSE COALESCE(octet_length(content::text) > {MAX_PACK_BYTES},false) END AS oversized \
             FROM candidates), \
             bounded AS MATERIALIZED (SELECT room_id,event_type,state_key,event_id, \
             CASE WHEN NOT oversized THEN content END AS content,oversized FROM measured), \
             selected AS (SELECT * FROM bounded \
             ORDER BY COALESCE(room_id,''),event_type,COALESCE(state_key,'') LIMIT $7) \
             SELECT authorized.joined,selected.* FROM authorized LEFT JOIN selected ON authorized.joined \
             ORDER BY COALESCE(room_id,''),event_type,COALESCE(state_key,'')"
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
        for r in rows {
            joined = r.try_get("joined")?;
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
        Ok((joined, sources))
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
                let (_, rows) = self
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
