//! Discovery for custom-reaction pickers, backed by current cached state.

use axon_core::image_pack::{parse_image_pack, MAX_ROOM_PACKS};
use axon_store::Store;
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::extract::Path;
use crate::response::{ApiError, ApiResponse};

/// Current-room and personal emoticon packs, as known to Axon's sync cache.
/// Empty results do not prove absence upstream while initial sync is pending.
#[derive(Debug, Serialize, ToSchema)]
pub struct RoomEmojiPacksDto {
    pub packs: Vec<EmojiPackDto>,
    /// Invalid/oversized content was omitted, or a pack/image limit was reached.
    pub partial: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EmojiPackDto {
    /// Null for the account's personal pack; otherwise the current room ID.
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
}

/// List custom emoji available from the current room and personal pack.
/// Supports `m.room.image_pack`, legacy `im.ponies.room_emotes`, and personal
/// `im.ponies.user_emotes`. Stable room tuples supersede legacy tuples with
/// the same state key. Personal packs precede room packs; names are not merged.
/// Sticker-only entries are excluded. Other-room subscriptions and canonical
/// space inheritance are not included. Requires positive joined membership.
/// This is a local cache read, not a homeserver request or an authoritative
/// statement that no packs exist upstream. Re-read on picker open/reconnect.
/// At most 64 room packs plus one personal pack, 128 KiB per source, 512 entries
/// examined per pack, and 1000 returned images across packs. `partial` signals
/// omitted invalid/oversized data or truncation. All names are untrusted text.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/emoji-packs",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    responses(
        (status = 200, description = "Cached personal and current-room emoji packs", body = ApiResponse<RoomEmojiPacksDto>),
        (status = 401, description = "Missing or invalid bearer token", body = crate::response::ErrorResponse),
        (status = 404, description = "Account inactive or joined room not known locally", body = crate::response::ErrorResponse),
        (status = 500, description = "Internal database failure", body = crate::response::ErrorResponse),
    ),
    tag = "rooms",
)]
pub async fn room_emoji_packs(
    State(store): State<Store>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<RoomEmojiPacksDto>, ApiError> {
    let rows = store
        .image_pack_snapshot(account_id, &room_id)
        .await?
        .ok_or_else(|| ApiError::not_found("joined room not found"))?;
    let mut result = RoomEmojiPacksDto {
        packs: Vec::new(),
        partial: false,
    };
    let mut room_packs = 0;
    let mut remaining = 1000;
    for row in rows {
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
        let partial = pack.partial || pack.images.len() > remaining;
        result.partial |= partial;
        let images: Vec<_> = pack
            .images
            .into_iter()
            .take(remaining)
            .map(|image| CustomEmojiDto {
                shortcode: image.shortcode,
                url: image.url,
                body: image.body,
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
