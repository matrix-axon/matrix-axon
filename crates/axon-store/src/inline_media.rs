//! Bounded, parsed inline-image references, persisted atomically with events.

use ruma_html::{Html, NodeData};
use serde_json::Value;
use sqlx_core::row::Row;
use uuid::Uuid;

use crate::{Store, StoreError};

const MAX_HTML_BYTES: usize = 65_536;
const MAX_IMAGES: usize = 64;

pub(crate) fn inline_media_urls(event_type: &str, content: Option<&Value>) -> Vec<String> {
    if event_type != "m.room.message" {
        return Vec::new();
    }
    let Some(content) = content else {
        return Vec::new();
    };
    let mut urls = Vec::new();
    let part = if content["m.relates_to"]["rel_type"] == "m.replace" {
        &content["m.new_content"]
    } else {
        content
    };
    if part["format"] != "org.matrix.custom.html" {
        return urls;
    }
    let Some(html) = part["formatted_body"]
        .as_str()
        .filter(|html| html.len() <= MAX_HTML_BYTES)
    else {
        return urls;
    };
    let document = Html::parse(html);
    document.sanitize();
    let mut pending: Vec<_> = document.children().collect();
    pending.reverse();
    while let Some(node) = pending.pop() {
        if let NodeData::Element(element) = node.data() {
            if element.name.local.as_ref() == "img" {
                let attrs = element.attrs.borrow();
                if let Some(src) = attrs.iter().find(|attr| {
                    attr.name.ns.as_ref().is_empty() && attr.name.local.as_ref() == "src"
                }) {
                    let url = src.value.as_ref();
                    if url.len() <= 2048
                        && axon_core::media::parse_mxc(url).is_some()
                        && !urls.iter().any(|old| old == url)
                    {
                        urls.push(url.to_owned());
                        if urls.len() == MAX_IMAGES {
                            return urls;
                        }
                    }
                }
            }
        }
        let mut children: Vec<_> = node.children().collect();
        children.reverse();
        pending.extend(children);
    }
    urls
}

impl Store {
    /// Authorize the image references in the same effective content timelines
    /// render, using the shared edit/redaction rules. The overall deadline also
    /// includes pool acquisition; the pool retains its configured SQL deadlines.
    pub async fn inline_message_authorizes_media(
        &self,
        account_id: Uuid,
        mxc: &str,
    ) -> Result<bool, StoreError> {
        if mxc.len() > 2048 || axon_core::media::parse_mxc(mxc).is_none() {
            return Ok(false);
        }
        let visible = crate::event_visibility::media_room_is_visible("e");
        let redacted = crate::event_visibility::event_is_redacted("e");
        let edits = crate::events::latest_edit_lateral();
        let sql = format!(
            "WITH candidates AS NOT MATERIALIZED (SELECT account_id, event_id, relates_to FROM events \
             WHERE account_id = $1 AND event_type = 'm.room.message' \
               AND inline_media_urls <> '{{}}' AND inline_media_urls @> ARRAY[$2]::text[]) \
             SELECT EXISTS (SELECT 1 FROM candidates candidate \
             JOIN events e ON e.account_id = candidate.account_id AND e.event_id = \
                 CASE WHEN candidate.relates_to->>'rel_type' = 'm.replace' \
                      THEN candidate.relates_to->>'event_id' ELSE candidate.event_id END \
             {edits} \
             WHERE e.event_type = 'm.room.message' AND e.content IS NOT NULL \
               AND e.raw_event->>'state_key' IS NULL \
               AND e.relates_to->>'rel_type' IS DISTINCT FROM 'm.replace' \
               AND {visible} AND NOT {redacted} \
               AND CASE WHEN edit.new_content IS NOT NULL THEN edit.inline_media_urls \
                        ELSE e.inline_media_urls END @> ARRAY[$2]::text[])"
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            sqlx_core::query_scalar::query_scalar(&sql)
                .bind(account_id)
                .bind(mxc)
                .fetch_one(&self.read_pool),
        )
        .await
        .map_err(|_| {
            StoreError::Sqlx(sqlx_core::Error::Protocol(
                "inline media authorization deadline exceeded".into(),
            ))
        })?
        .map_err(StoreError::from)
    }

    /// Repair one bounded historical batch using the isolated maintenance pool.
    /// NULL is the durable pending marker; a completed empty projection is '{}'.
    /// Row locks close races with decryption, deletion, and duplicate delivery.
    /// A canceled/timed-out batch rolls back and the next boot retries it.
    pub async fn backfill_inline_media_batch(&self) -> Result<usize, StoreError> {
        let repair = async {
            let mut tx = self.maintenance_pool.begin().await?;
            let rows = sqlx_core::query::query(
                "SELECT id, CASE WHEN octet_length(content::text) <= 131072 THEN content END AS content \
                 FROM events WHERE event_type='m.room.message' AND inline_media_urls IS NULL \
                 ORDER BY id LIMIT 32 FOR UPDATE",
            ).fetch_all(&mut *tx).await?;
            let proofs = crate::event_visibility::capture_redaction_proofs();
            let proof_sql = format!("WITH ins AS (SELECT * FROM events WHERE id=$1) {proofs}");
            for row in &rows {
                let id: i64 = row.try_get("id")?;
                sqlx_core::query::query(&proof_sql)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                let content: Option<Value> = row.try_get("content")?;
                let urls = inline_media_urls("m.room.message", content.as_ref());
                sqlx_core::query::query("UPDATE events SET inline_media_urls=$2 WHERE id=$1")
                    .bind(id)
                    .bind(urls)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            Ok(rows.len())
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), repair)
            .await
            .map_err(|_| {
                StoreError::Sqlx(sqlx_core::Error::Protocol(
                    "inline media backfill deadline exceeded".into(),
                ))
            })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn authorization_deadline_includes_pool_acquisition_and_releases_the_waiter() {
        let store = Store::connect_with_config(&axon_core::DatabaseConfig {
            url: std::env::var("DATABASE_URL").unwrap(),
            max_connections: 1,
            timeouts: axon_core::DatabaseTimeouts {
                acquire_secs: 30,
                ..Default::default()
            },
        })
        .await
        .unwrap();
        let lease = store.read_pool.acquire().await.unwrap();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(12),
            store.inline_message_authorizes_media(Uuid::nil(), "mxc://example.org/deadline"),
        )
        .await
        .expect("authorization must have its own overall deadline");
        assert!(matches!(
            outcome,
            Err(StoreError::Sqlx(sqlx_core::Error::Protocol(_)))
        ));
        drop(lease);
        assert!(!store
            .inline_message_authorizes_media(Uuid::nil(), "mxc://example.org/deadline")
            .await
            .unwrap());
    }

    #[test]
    fn keeps_the_first_distinct_images_in_document_order() {
        let html = (0..100)
            .map(|i| format!("<span><img src='mxc://hs/{i}'></span>"))
            .collect::<String>();
        let urls = inline_media_urls(
            "m.room.message",
            Some(&json!({"format":"org.matrix.custom.html", "formatted_body": html})),
        );
        assert_eq!(
            urls,
            (0..64).map(|i| format!("mxc://hs/{i}")).collect::<Vec<_>>()
        );
    }

    #[test]
    fn ignores_replacement_content_on_non_edits() {
        let content = json!({"m.new_content":{"format":"org.matrix.custom.html", "formatted_body":"<img src='mxc://hs/hidden'>"}});
        assert!(inline_media_urls("m.room.message", Some(&content)).is_empty());
    }

    #[test]
    fn parses_images_and_entities_but_not_comments_links_scripts_or_reply_fallbacks() {
        let content = json!({"format":"org.matrix.custom.html", "formatted_body":
            "<!-- <img src='mxc://hs/comment'> --><script><img src='mxc://hs/script'></script><mx-reply><img src='mxc://hs/reply'></mx-reply><a href='mxc://hs/link'>link</a><img src='mxc://hs/a&amp;b'><img src='https://tracker/image'>"});
        assert_eq!(
            inline_media_urls("m.room.message", Some(&content)),
            ["mxc://hs/a&b"]
        );
        assert!(inline_media_urls("m.room.topic", Some(&content)).is_empty());
        assert!(inline_media_urls(
            "m.room.message",
            Some(&json!({"formatted_body":"<img src='mxc://hs/a'>"}))
        )
        .is_empty());
    }

    #[test]
    fn extracts_edits_and_bounds_resources() {
        let image = "<img src='mxc://hs/a'>";
        let edit = json!({"m.relates_to":{"rel_type":"m.replace"}, "m.new_content":{"format":"org.matrix.custom.html", "formatted_body": image}});
        assert_eq!(
            inline_media_urls("m.room.message", Some(&edit)),
            ["mxc://hs/a"]
        );
        let oversized = json!({"format":"org.matrix.custom.html", "formatted_body": image.repeat(MAX_HTML_BYTES)});
        assert!(inline_media_urls("m.room.message", Some(&oversized)).is_empty());
        let images = (0..100)
            .map(|i| format!("<img src='mxc://hs/{i}'>"))
            .collect::<String>();
        assert_eq!(
            inline_media_urls(
                "m.room.message",
                Some(&json!({"format":"org.matrix.custom.html", "formatted_body":images}))
            )
            .len(),
            MAX_IMAGES
        );
    }
}
