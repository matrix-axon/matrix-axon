//! Discovery for custom reaction and sticker pickers, backed by cached state.

use axon_core::image_pack::{parse_image_pack, MAX_ROOM_PACKS};
use axon_store::Store;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::dto::StickerInfoDto;
use crate::extract::{Path, Query};
use crate::response::{ApiError, ApiResponse};

#[derive(Debug, Clone, Copy, Default, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImagePackUsage {
    #[default]
    Emoticon,
    Sticker,
}

#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct ImagePackQuery {
    /// Default: emoticon. Sticker mode includes sticker-only and mixed entries.
    #[serde(default)]
    pub usage: ImagePackUsage,
}

/// Current-room, canonical ancestor, and personal image packs, as known to Axon's sync cache.
/// Empty results do not prove absence upstream while initial sync is pending.
#[derive(Debug, Serialize, ToSchema)]
pub struct RoomEmojiPacksDto {
    pub packs: Vec<EmojiPackDto>,
    /// Invalid/oversized content was omitted, a pack/image/ancestor limit was reached, or a cycle was found.
    pub partial: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EmojiPackDto {
    /// Null for the account's personal pack; otherwise its source room or ancestor space ID.
    pub room_id: Option<String>,
    /// Source state/account-data type, preserving stable or legacy identifiers.
    pub event_type: String,
    /// Null for personal account data. Empty string is a valid room pack key.
    pub state_key: Option<String>,
    /// Room state event ID. Personal account data has no event ID.
    pub event_id: Option<String>,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub images: Vec<CustomEmojiDto>,
    pub partial: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CustomEmojiDto {
    /// Untrusted display text, not HTML. Send alongside the MXC reaction key.
    pub shortcode: String,
    /// Validated MXC URI; fetch through the authenticated media proxy.
    pub url: String,
    /// Optional plain-text description, distinct from the search shortcode.
    pub body: Option<String>,
    pub emoticon: bool,
    pub sticker: bool,
    pub info: Option<StickerInfoDto>,
}

/// List custom emoji available from the current room, canonical ancestor spaces, and personal pack.
/// Supports `m.room.image_pack`, legacy `im.ponies.room_emotes`, and personal
/// `im.ponies.user_emotes`. Stable room tuples supersede legacy tuples with
/// the same state key. Personal packs precede room packs, then nearest ancestors; names are not merged.
/// Sticker-only entries are excluded by default; `usage=sticker` selects
/// sticker-capable images instead. Up to 16 joined canonical ancestor spaces
/// are inherited through locally validated parent relationships. Other-room
/// subscriptions are not included. Requires positive joined membership.
/// This is a local cache read, not a homeserver request or an authoritative
/// statement that no packs exist upstream. Re-read on picker open/reconnect.
/// At most 64 room packs plus one personal pack, 128 KiB per source, 512 entries
/// examined per pack, and 1000 returned images across packs. `partial` signals
/// omitted invalid/oversized data or truncation. All names are untrusted text.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/emoji-packs",
    params(
        ImagePackQuery,
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    responses(
        (status = 200, description = "Cached personal, current-room, and canonical ancestor emoji packs", body = ApiResponse<RoomEmojiPacksDto>),
        (status = 401, description = "Missing or invalid bearer token", body = crate::response::ErrorResponse),
        (status = 404, description = "Account inactive or joined room not known locally", body = crate::response::ErrorResponse),
        (status = 500, description = "Internal database failure", body = crate::response::ErrorResponse),
    ),
    tag = "rooms",
)]
pub async fn room_emoji_packs(
    State(store): State<Store>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
    Query(query): Query<ImagePackQuery>,
) -> Result<ApiResponse<RoomEmojiPacksDto>, ApiError> {
    let snapshot = store
        .image_pack_snapshot(account_id, &room_id)
        .await?
        .ok_or_else(|| ApiError::not_found("joined room not found"))?;
    if snapshot.partial {
        tracing::debug!(%account_id, %room_id, "image pack ancestry traversal reached a cycle or depth limit");
    }
    let mut result = RoomEmojiPacksDto {
        packs: Vec::new(),
        partial: snapshot.partial,
    };
    let mut room_packs = 0;
    let mut remaining = 1000;
    for row in snapshot.rows {
        if remaining == 0 {
            result.partial = true;
            break;
        }
        if row.room_id.is_some() {
            room_packs += 1;
            if room_packs > MAX_ROOM_PACKS {
                result.partial = true;
                break;
            }
        }
        let Some(pack) = row.content.as_ref().and_then(parse_image_pack) else {
            result.partial = true;
            tracing::debug!(%account_id, %room_id, event_id = ?row.event_id, oversized = row.oversized, "omitted invalid or unavailable emoji pack");
            continue;
        };
        let selected: Vec<_> = pack
            .images
            .into_iter()
            .filter(|image| match query.usage {
                ImagePackUsage::Emoticon => image.emoticon,
                ImagePackUsage::Sticker => image.sticker,
            })
            .collect();
        let partial = pack.partial || selected.len() > remaining;
        result.partial |= partial;
        let images: Vec<_> = selected
            .into_iter()
            .take(remaining)
            .map(|image| CustomEmojiDto {
                shortcode: image.shortcode,
                url: image.url,
                body: image.body,
                emoticon: image.emoticon,
                sticker: image.sticker,
                info: image
                    .info
                    .and_then(|info| serde_json::from_value(info).ok()),
            })
            .collect();
        remaining -= images.len();
        result.packs.push(EmojiPackDto {
            room_id: row.room_id,
            event_type: row.event_type,
            state_key: row.state_key,
            event_id: row.event_id,
            display_name: pack.display_name,
            avatar_url: pack.avatar_url,
            images,
            partial,
        });
    }
    Ok(ApiResponse::new(result))
}
