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
const SOURCES: &str = "WITH sources AS ( \
    SELECT rs.room_id, rs.event_type, rs.state_key, rs.event_id, rs.content \
    FROM room_state rs JOIN accounts ac ON ac.account_id = rs.account_id \
    JOIN room_state membership ON membership.account_id = rs.account_id \
      AND membership.room_id = rs.room_id AND membership.event_type = 'm.room.member' \
      AND membership.state_key = ac.user_id AND membership.content->>'membership' = 'join' \
    WHERE rs.account_id = $1 AND ac.state = 'active' \
      AND rs.event_type IN ('m.room.image_pack', 'im.ponies.room_emotes') \
      AND rs.redacted IS DISTINCT FROM true \
      AND (rs.event_type = 'm.room.image_pack' OR NOT EXISTS ( \
        SELECT 1 FROM room_state stable WHERE stable.account_id = rs.account_id \
        AND stable.room_id = rs.room_id AND stable.event_type = 'm.room.image_pack' \
        AND stable.state_key = rs.state_key)) \
    UNION ALL \
    SELECT NULL::text, ad.event_type, NULL::text, NULL::text, ad.content \
    FROM account_data ad JOIN accounts ac ON ac.account_id = ad.account_id \
    WHERE ad.account_id = $1 AND ac.state = 'active' AND ad.room_id = '' \
      AND ad.event_type = 'im.ponies.user_emotes' \
) ";

impl Store {
    /// True only with positive current membership evidence for this account.
    pub async fn image_pack_room_joined(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<bool, StoreError> {
        Ok(sqlx_core::query_scalar::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM room_state rs JOIN accounts ac \
             ON ac.account_id = rs.account_id WHERE rs.account_id = $1 \
             AND rs.room_id = $2 AND rs.event_type = 'm.room.member' \
             AND rs.state_key = ac.user_id AND rs.content->>'membership' = 'join' \
             AND ac.state = 'active')",
        )
        .bind(account_id)
        .bind(room_id)
        .fetch_one(&self.read_pool)
        .await?)
    }

    /// At most 66 source rows, including room packs and the personal pack.
    /// Callers expose at most 64 room tuples and use lookahead for truncation.
    /// Content is capped at 128 KiB in SQL before transfer/JSON decoding.
    pub async fn image_pack_sources(
        &self,
        account_id: Uuid,
        room_id: Option<&str>,
        mxc: Option<&str>,
    ) -> Result<Vec<ImagePackRow>, StoreError> {
        let sql = format!(
            "{SOURCES} SELECT room_id,event_type,state_key,event_id, \
             CASE WHEN octet_length(content::text) <= {MAX_PACK_BYTES} THEN content END AS content, \
             COALESCE(octet_length(content::text) > {MAX_PACK_BYTES}, false) AS oversized \
             FROM sources WHERE ($2::text IS NULL OR room_id = $2 OR room_id IS NULL) \
             AND ($3::text IS NULL OR CASE WHEN octet_length(content::text) <= {MAX_PACK_BYTES} \
                  THEN content#>>'{{pack,avatar_url}}' = $3 OR \
                       jsonb_path_exists(content, '$.images.*.url ? (@ == $mxc)', \
                       jsonb_build_object('mxc', $3::text)) ELSE false END) \
             ORDER BY room_id NULLS FIRST,event_type,state_key LIMIT $4"
        );
        let rows = sqlx_core::query::query(&sql)
            .bind(account_id)
            .bind(room_id)
            .bind(mxc)
            .bind((MAX_ROOM_PACKS + 2) as i64)
            .fetch_all(&self.read_pool)
            .await?;
        rows.into_iter()
            .map(|r| {
                Ok(ImagePackRow {
                    room_id: r.try_get("room_id")?,
                    event_type: r.try_get("event_type")?,
                    state_key: r.try_get("state_key")?,
                    event_id: r.try_get("event_id")?,
                    content: r.try_get("content")?,
                    oversized: r.try_get("oversized")?,
                })
            })
            .collect()
    }

    /// Only parsed emoticon entries in current, accessible packs authorize
    /// previews. Old events, redacted packs and malformed entries do not.
    pub async fn image_pack_authorizes_media(
        &self,
        account_id: Uuid,
        mxc: &str,
    ) -> Result<bool, StoreError> {
        if axon_core::media::parse_mxc(mxc).is_none() {
            return Ok(false);
        }
        Ok(self
            .image_pack_sources(account_id, None, Some(mxc))
            .await?
            .into_iter()
            .filter_map(|r| r.content.and_then(|c| parse_image_pack(&c)))
            .any(|p| {
                p.avatar_url.as_deref() == Some(mxc)
                    || p.images.iter().any(|image| image.url == mxc)
            }))
    }
}
