//! Bounded, parsed inline-image references, persisted atomically with events.

use ruma_html::{Html, NodeData};
use serde_json::Value;
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
    for part in [Some(content), content.get("m.new_content")]
        .into_iter()
        .flatten()
    {
        if part["format"] != "org.matrix.custom.html" {
            continue;
        }
        let Some(html) = part["formatted_body"]
            .as_str()
            .filter(|html| html.len() <= MAX_HTML_BYTES)
        else {
            continue;
        };
        let document = Html::parse(html);
        document.sanitize();
        let mut pending: Vec<_> = document.children().collect();
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
            pending.extend(node.children());
        }
    }
    urls
}

impl Store {
    /// Authorize plain raster media referenced by a newly ingested message.
    /// Pack membership is irrelevant once the reference has been received.
    pub async fn inline_message_authorizes_media(
        &self,
        account_id: Uuid,
        mxc: &str,
    ) -> Result<bool, StoreError> {
        if axon_core::media::parse_mxc(mxc).is_none() {
            return Ok(false);
        }
        Ok(sqlx_core::query_scalar::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM events e \
             WHERE e.account_id = $1 AND e.event_type = 'm.room.message' \
               AND e.inline_media_urls <> '{}' \
               AND e.inline_media_urls @> ARRAY[$2]::text[] AND e.content IS NOT NULL \
               AND e.raw_event->>'state_key' IS NULL \
               AND NOT EXISTS (SELECT 1 FROM room_state rs JOIN accounts a ON a.account_id = rs.account_id \
                   WHERE rs.account_id = e.account_id AND rs.room_id = e.room_id \
                     AND rs.event_type = 'm.room.member' AND rs.state_key = a.user_id \
                     AND rs.content->>'membership' IN ('leave', 'ban')) \
               AND NOT EXISTS (SELECT 1 FROM events r \
                   WHERE r.account_id = e.account_id AND r.event_type = 'm.room.redaction' \
                     AND (r.redacts = e.event_id OR (e.relates_to->>'rel_type' = 'm.replace' \
                         AND r.redacts = e.relates_to->>'event_id'))))"
        ).bind(account_id).bind(mxc).fetch_one(&self.read_pool).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        let edit =
            json!({"m.new_content":{"format":"org.matrix.custom.html", "formatted_body": image}});
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
