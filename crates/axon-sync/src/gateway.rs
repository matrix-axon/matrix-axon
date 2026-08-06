//! The message gateway: turns high-level mutation requests into matrix-rust-sdk
//! send calls on the right account's [`Client`](matrix_sdk::Client).
//!
//! [`SdkGateway`] owns *message semantics only* — building the ruma content for a
//! send / edit / redact / react and issuing it. It resolves the account's client
//! through the [`ClientManager`] (lazily connecting if needed) but knows nothing
//! about connection retry or caching; that is the manager's job. This is the
//! concrete capability `axon-server` adapts onto the API layer's `MessageSender`
//! port — `axon-api` never sees this type or any SDK type.

//!
//! Each method returns the resulting Matrix event id as a `String`. Errors are
//! [`GatewayError`], chosen so the composition-root adapter can map them onto
//! HTTP status without this crate knowing about HTTP.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axon_core::{
    CreateRoomRequest, Formatted, MatrixProfile, MediaAttachment, MediaSendKind, PowerLevelChanges,
    PublicRoomSummary, PublicRoomsPage, PublicRoomsQuery, Relation, ResolvedPowerLevels,
    RoomPreset,
};
use matrix_sdk::attachment::{AttachmentConfig, AttachmentInfo, BaseFileInfo, BaseImageInfo};
use matrix_sdk::room::reply::{EnforceThread, Reply};
use matrix_sdk::room::Receipts;
use matrix_sdk::ruma::api::client::directory::get_public_rooms_filtered;
use matrix_sdk::ruma::api::client::profile::{AvatarUrl, DisplayName};
use matrix_sdk::ruma::api::client::room::{create_room, Visibility};
use matrix_sdk::ruma::api::error::ErrorKind;
use matrix_sdk::ruma::directory::Filter;
use matrix_sdk::ruma::events::reaction::ReactionEventContent;
use matrix_sdk::ruma::events::relation::{Annotation, Thread};
use matrix_sdk::ruma::events::room::encryption::RoomEncryptionEventContent;
use matrix_sdk::ruma::events::room::message::{
    AddMentions, FileInfo, FileMessageEventContent, ImageMessageEventContent, MessageType,
    Relation as MessageRelation, ReplyWithinThread, RoomMessageEventContent,
    TextMessageEventContent,
};
use matrix_sdk::ruma::events::room::power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent};
use matrix_sdk::ruma::events::room::{ImageInfo, MediaSource};
use matrix_sdk::ruma::events::tag::{TagInfo, TagName, UserTagName};
use matrix_sdk::ruma::events::{InitialStateEvent, StateEventType};
use matrix_sdk::ruma::{
    EventId, Int, OwnedEventId, OwnedRoomOrAliasId, OwnedServerName, OwnedUserId, RoomId,
    RoomOrAliasId, ServerName, UInt, UserId,
};
use matrix_sdk::{Room, RoomState};
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use crate::error::GatewayError;
use crate::manager::ClientManager;

/// Map an SDK error from a send/fetch into a [`GatewayError`]. A homeserver
/// `M_FORBIDDEN` (e.g. redacting without the required power level) becomes
/// [`GatewayError::Forbidden`] so it surfaces as `403`; `M_BAD_STATE` (e.g.
/// unbanning a user who isn't banned, ADR 0068 M19b's `kick`/`ban`/`unban`) is
/// a caller mistake, not a homeserver fault, so it becomes
/// [`GatewayError::Invalid`] (`400`) rather than falling into the generic
/// upstream `502`; everything else is an upstream failure.
fn map_sdk_err(err: matrix_sdk::Error) -> GatewayError {
    match err.client_api_error_kind() {
        Some(ErrorKind::Forbidden) => GatewayError::Forbidden(err.to_string()),
        Some(ErrorKind::BadState) => GatewayError::Invalid(err.to_string()),
        _ => GatewayError::Upstream(err.to_string()),
    }
}

/// Reject an operation on a room that isn't `Joined` (ADR 0068, M19d/M19e).
/// For a state *write*, the SDK's own `send_state_event`/`upload_avatar`
/// paths call `ensure_room_joined` internally and fail with a local
/// `Error::WrongRoomState` when they aren't — an SDK-local precondition
/// failure, not a homeserver response, so it carries no
/// `client_api_error_kind` and `map_sdk_err`'s catch-all would otherwise
/// surface it as a misleading `502` instead of a clean `400`. Shared by every
/// M19d state write (`set_name`, `set_topic`, `set_avatar`, `remove_avatar`)
/// and M19e's `set_power_levels` rather than duplicated per method; `set_tag`/
/// `remove_tag` don't call this because they write room account data via a
/// direct `client.send`, which the homeserver accepts regardless of local
/// join state. M19e's `power_levels` *read* also calls this — not because
/// the SDK needs it, but because without it `Room::power_levels()` would
/// happily return a left/banned/invited room's stale cached state, which is
/// exactly the kind of inconsistency-with-the-write-path this guard closes
/// everywhere else.
fn ensure_joined(room: &Room) -> Result<(), GatewayError> {
    if room.state() != RoomState::Joined {
        return Err(GatewayError::Invalid(format!(
            "cannot write room state in room state {:?}; the room must be joined",
            room.state()
        )));
    }
    Ok(())
}

/// Merge a [`PowerLevelChanges`] request into a resolved [`RoomPowerLevels`]
/// in place (ADR 0068, M19e): role thresholds first, then per-user levels.
/// Deliberately doesn't reuse matrix-sdk's own `RoomPowerLevelChanges` +
/// `apply()` — that type has no `users` field, so a caller wanting both a
/// role-threshold change and a promote/demote would need matrix-sdk's two
/// separate `apply_power_level_changes`/`update_power_levels` calls, each an
/// independent read-modify-send round trip. Two calls would fire two
/// `m.room.power_levels` state events for one client request and let
/// [`check_self_demotion_guardrail`] see a stale intermediate state between
/// them, so this merges both axes into one in-memory copy before a single
/// send in [`SdkGateway::set_power_levels`].
fn merge_power_level_changes(
    power_levels: &mut RoomPowerLevels,
    changes: &PowerLevelChanges,
) -> Result<(), GatewayError> {
    let to_level = |v: i64| -> Result<Int, GatewayError> {
        Int::try_from(v).map_err(|e| GatewayError::Invalid(format!("power level: {e}")))
    };
    if let Some(v) = changes.ban {
        power_levels.ban = to_level(v)?;
    }
    if let Some(v) = changes.invite {
        power_levels.invite = to_level(v)?;
    }
    if let Some(v) = changes.kick {
        power_levels.kick = to_level(v)?;
    }
    if let Some(v) = changes.redact {
        power_levels.redact = to_level(v)?;
    }
    if let Some(v) = changes.events_default {
        power_levels.events_default = to_level(v)?;
    }
    if let Some(v) = changes.state_default {
        power_levels.state_default = to_level(v)?;
    }
    if let Some(v) = changes.users_default {
        power_levels.users_default = to_level(v)?;
    }
    for (user_id, level) in &changes.users {
        let user_id = parse_user_id(user_id)?;
        let level = to_level(*level)?;
        // Mirrors matrix-sdk's own `Room::update_power_levels`: a level equal
        // to the room's default carries no information as an explicit
        // override, so drop the entry instead of writing a needless one into
        // the persisted event content.
        if level == power_levels.users_default {
            power_levels.users.remove(&user_id);
        } else {
            power_levels.users.insert(user_id, level);
        }
    }
    Ok(())
}

/// Reject a power-level write that would *itself* drop the caller's own
/// resolved level below what's needed to send another `m.room.power_levels`
/// event, unless `acknowledge` is set (ADR 0068, M19e). This is the one
/// guardrail in the whole M19 batch that `map_sdk_err`'s `M_FORBIDDEN` →
/// `403` mapping can't provide: the homeserver accepts a self-demoting
/// power-level write at the protocol level, so it succeeds and permanently
/// strands the caller with no way to self-correct — there is no error for
/// Axon to translate, only a state to refuse to reach.
///
/// Takes both `before` (the room's power levels prior to this request) and
/// `merged` (the same, with this request's changes applied) rather than just
/// `merged`: comparing only the resulting level would also reject a request
/// from a caller who *already* couldn't self-correct before this write ever
/// happened (e.g. a no-op body, or a change that doesn't touch their own
/// level at all) — a real case the gateway's own test suite caught. That
/// caller isn't stranded *by this request*, so `acknowledge_self_demotion`
/// wouldn't actually rescue them (the homeserver will reject the send with
/// its own `M_FORBIDDEN` regardless, since it enforces the identical
/// sender-level-vs-required check against the room's real state), and
/// telling them to set it would be misleading. Only a transition from
/// "could self-correct" to "can't" is this guardrail's concern; an
/// already-insufficient caller is left to the ordinary `map_sdk_err`
/// `Forbidden` → `403` path.
fn check_self_demotion_guardrail(
    before: &RoomPowerLevels,
    merged: &RoomPowerLevels,
    own_user_id: &UserId,
    acknowledge: bool,
) -> Result<(), GatewayError> {
    if acknowledge {
        return Ok(());
    }

    let required_before = before.for_state(StateEventType::RoomPowerLevels);
    let own_level_before = before.for_user(own_user_id);
    if own_level_before < required_before {
        return Ok(());
    }

    let required_after = merged.for_state(StateEventType::RoomPowerLevels);
    let own_level_after = merged.for_user(own_user_id);
    if own_level_after < required_after {
        return Err(GatewayError::Invalid(format!(
            "this change would drop your own power level ({own_level_after:?}) below \
             {required_after} (the level required to send m.room.power_levels), leaving no \
             way to self-correct; set acknowledge_self_demotion to proceed anyway"
        )));
    }
    Ok(())
}

/// Parse a Matrix event id targeted by a mutation, mapping a malformed one to
/// [`GatewayError::Invalid`] (a clean `400`) instead of letting it reach the
/// homeserver. Shared by every method that addresses an existing event: edit,
/// redact, react, and the read-receipt send.
fn parse_event_id(event_id: &str) -> Result<OwnedEventId, GatewayError> {
    EventId::parse(event_id).map_err(|e| GatewayError::Invalid(format!("event id: {e}")))
}

/// Parse a Matrix user id targeted by a membership mutation (invite/kick/
/// ban/unban), mapping a malformed one to [`GatewayError::Invalid`] instead of
/// letting it reach the homeserver.
fn parse_user_id(user_id: &str) -> Result<OwnedUserId, GatewayError> {
    UserId::parse(user_id).map_err(|e| GatewayError::Invalid(format!("user id: {e}")))
}

/// Parse a room id or alias targeted by a room-entry mutation (join/knock;
/// ADR 0068, M19c), mapping a malformed one to [`GatewayError::Invalid`]
/// instead of letting it reach the homeserver. `RoomOrAliasId` accepts both a
/// bare room id (`!...`) and an alias (`#...`), so callers don't need to
/// distinguish the two up front.
fn parse_room_id_or_alias(id: &str) -> Result<OwnedRoomOrAliasId, GatewayError> {
    RoomOrAliasId::parse(id).map_err(|e| GatewayError::Invalid(format!("room id or alias: {e}")))
}

/// Shared `ServerName::parse` call behind [`parse_server_name`] and
/// [`parse_server_names`] — the two differ only in how they format the
/// resulting `GatewayError` (one server name vs. naming which entry in a
/// list failed), so both build on this one parse instead of each calling
/// `ServerName::parse` independently.
fn try_parse_server_name(name: &str) -> Result<OwnedServerName, matrix_sdk::ruma::IdParseError> {
    ServerName::parse(name)
}

/// Parse a single target server name for a directory search (ADR 0068,
/// M19f's `public_rooms_filtered`'s `server` field), mapping a malformed one
/// to [`GatewayError::Invalid`] instead of letting it reach the homeserver.
fn parse_server_name(name: &str) -> Result<OwnedServerName, GatewayError> {
    try_parse_server_name(name).map_err(|e| GatewayError::Invalid(format!("server: {e}")))
}

/// Parse the `server_names` hints a join/knock caller supplies for federation
/// resolution (ruma's `via`) of an alias/id this account's client has no
/// direct path to.
fn parse_server_names(names: &[String]) -> Result<Vec<OwnedServerName>, GatewayError> {
    names
        .iter()
        .map(|name| {
            try_parse_server_name(name)
                .map_err(|e| GatewayError::Invalid(format!("server name {name:?}: {e}")))
        })
        .collect()
}

/// Upper bound on a room tag's wire-form byte length (ADR 0068, M19d review).
/// Not itself a hard limit the Matrix spec text calls out for `m.tag`, but a
/// deterministic Axon-side cap so a pathologically long custom tag name fails
/// as a clean `400` here rather than round-tripping to the homeserver and
/// coming back as a homeserver-dependent rejection — the same reasoning
/// behind this crate's other upload-metadata caps
/// (`MAX_FILENAME_CHARS`/`MAX_CONTENT_TYPE_CHARS` in
/// `axon-api::routes::uploads`), whose 255-byte value this mirrors.
const MAX_TAG_NAME_BYTES: usize = 255;

/// Parse a room tag's wire form (ADR 0068, M19d) into a [`TagName`],
/// accepting only the three well-known names (`m.favourite`, `m.lowpriority`,
/// `m.server_notice`) and `u.`-prefixed custom tags, rejecting anything else
/// as [`GatewayError::Invalid`]. This validation can't be left to `TagName`
/// itself: its `From<T: AsRef<str>>` conversion is infallible — any
/// unrecognized string silently becomes `TagName::_Custom` — so an unvalidated
/// tag would round-trip to the homeserver as a made-up, non-namespaced tag
/// instead of failing clean at the API boundary.
///
/// The custom-tag branch delegates its `u.`-prefix check to `UserTagName`'s
/// own `FromStr` rather than re-deriving it, so a future change to what ruma
/// considers a valid custom tag is picked up here automatically. `UserTagName`
/// alone accepts a bare `u.` (any string starting with `u.` parses, including
/// itself), which is why this narrows further with its own `len()` check: a
/// nameless custom tag has nothing to sort/display by, so it's still rejected
/// — deliberately stricter than ruma, not a plain passthrough of it.
fn parse_tag_name(tag: &str) -> Result<TagName, GatewayError> {
    let invalid = || {
        GatewayError::Invalid(format!(
            "tag: must be m.favourite, m.lowpriority, m.server_notice, or u.<name>, got {tag:?}"
        ))
    };
    if tag.len() > MAX_TAG_NAME_BYTES {
        return Err(GatewayError::Invalid(format!(
            "tag: must be at most {MAX_TAG_NAME_BYTES} bytes, got {}",
            tag.len()
        )));
    }
    match tag {
        "m.favourite" | "m.lowpriority" | "m.server_notice" => Ok(TagName::from(tag)),
        _ => tag
            .parse::<UserTagName>()
            .ok()
            .filter(|_| tag.len() > "u.".len())
            .map(TagName::User)
            .ok_or_else(invalid),
    }
}

/// Validate a room tag's `order` (ADR 0068, M19d review) against the Matrix
/// spec's documented range for `m.tag`'s `order` field: a number in `[0, 1]`.
/// Ruma's own `TagInfo` carries `order` as a bare `Option<f64>` with no range
/// check, so an out-of-range value would otherwise round-trip to the
/// homeserver instead of failing clean at the API boundary. `NaN` and
/// infinities fail every comparison against the range and so are rejected by
/// the same guard without a separate check.
fn validate_tag_order(order: Option<f64>) -> Result<Option<f64>, GatewayError> {
    match order {
        Some(value) if !(0.0..=1.0).contains(&value) => Err(GatewayError::Invalid(format!(
            "tag order must be between 0 and 1, got {value}"
        ))),
        _ => Ok(order),
    }
}

fn map_preset(preset: RoomPreset) -> create_room::v3::RoomPreset {
    match preset {
        RoomPreset::Private => create_room::v3::RoomPreset::PrivateChat,
        RoomPreset::Public => create_room::v3::RoomPreset::PublicChat,
        RoomPreset::TrustedPrivate => create_room::v3::RoomPreset::TrustedPrivateChat,
    }
}

/// Build the ruma create-room request from Axon's own request shape
/// (ADR 0068, M19c). `encrypted` becomes an `m.room.encryption` entry in
/// `initial_state` rather than a post-hoc `Room::enable_encryption()` call,
/// so an encrypted room is encrypted from its very first (`m.room.create`)
/// transaction — there is no window where the room exists but isn't yet
/// encrypted. Pure function: unit-testable without a live `Client`/`Room`.
fn build_create_room_request(
    request: CreateRoomRequest,
) -> Result<create_room::v3::Request, GatewayError> {
    let invite = request
        .invite
        .iter()
        .map(|user_id| parse_user_id(user_id))
        .collect::<Result<Vec<_>, _>>()?;
    let initial_state = if request.encrypted {
        vec![InitialStateEvent::with_empty_state_key(
            RoomEncryptionEventContent::with_recommended_defaults(),
        )
        .to_raw_any()]
    } else {
        Vec::new()
    };
    let mut built = create_room::v3::Request::new();
    built.name = request.name;
    built.topic = request.topic;
    built.invite = invite;
    built.is_direct = request.is_direct;
    built.visibility = if request.public {
        Visibility::Public
    } else {
        Visibility::Private
    };
    built.preset = request.preset.map(map_preset);
    built.initial_state = initial_state;
    Ok(built)
}

fn effective_mime(
    kind: MediaSendKind,
    content_type: Option<&str>,
) -> Result<mime::Mime, GatewayError> {
    let parsed = match content_type {
        Some(value) => value
            .parse::<mime::Mime>()
            .map_err(|e| GatewayError::Invalid(format!("content_type: {e}")))?,
        None => match kind {
            MediaSendKind::Image => mime::IMAGE_PNG,
            MediaSendKind::File => mime::APPLICATION_OCTET_STREAM,
        },
    };

    match kind {
        MediaSendKind::Image if parsed.type_() != mime::IMAGE => Err(GatewayError::Invalid(
            "image uploads must have an image/* content type".to_owned(),
        )),
        MediaSendKind::Image => Ok(parsed),
        // matrix-sdk chooses m.image/m.audio/m.video from the MIME major type.
        // M15 supports only explicit m.file for file uploads, so avoid those
        // major types even when the original Content-Type was specific.
        MediaSendKind::File
            if matches!(parsed.type_(), mime::IMAGE | mime::AUDIO | mime::VIDEO) =>
        {
            Ok(mime::APPLICATION_OCTET_STREAM)
        }
        MediaSendKind::File => Ok(parsed),
    }
}

fn attachment_info(kind: MediaSendKind, size_bytes: u64) -> Result<AttachmentInfo, GatewayError> {
    let size = UInt::new(size_bytes).ok_or_else(|| {
        GatewayError::Invalid("upload is too large for Matrix metadata".to_owned())
    })?;
    Ok(match kind {
        MediaSendKind::Image => AttachmentInfo::Image(BaseImageInfo {
            size: Some(size),
            ..BaseImageInfo::default()
        }),
        MediaSendKind::File => AttachmentInfo::File(BaseFileInfo { size: Some(size) }),
    })
}

/// Build the SDK [`Reply`] for a media send that carries a `reply_to` — a
/// plain reply, or an explicit reply scoped to a thread. Returns `None` for an
/// unrelated send. The `thread_root`-only "thread member, not a reply" shape
/// is *not* handled here: [`thread_member_root`] intercepts it before this
/// function is ever called, because the SDK's `Reply`/`make_for_thread` has
/// no way to express it without a fallback `m.in_reply_to` (see
/// [`SdkGateway::send_media`]).
fn attachment_reply(relation: Relation<'_>) -> Result<Option<Reply>, GatewayError> {
    let Some(reply_to) = relation
        .reply_to
        .map(|id| EventId::parse(id).map_err(|e| GatewayError::Invalid(format!("reply_to: {e}"))))
        .transpose()?
    else {
        return Ok(None);
    };

    let (enforce_thread, add_mentions) = if let Some(root) = relation.thread_root {
        EventId::parse(root).map_err(|e| GatewayError::Invalid(format!("thread_root: {e}")))?;
        (
            EnforceThread::Threaded(ReplyWithinThread::Yes),
            AddMentions::Yes,
        )
    } else {
        (EnforceThread::Unthreaded, AddMentions::Yes)
    };
    Ok(Some(Reply {
        event_id: reply_to,
        enforce_thread,
        add_mentions,
    }))
}

/// Whether `relation` is the "thread member, not a reply" shape (`thread_root`
/// set, `reply_to` absent) — the one shape [`attachment_reply`] can't build.
/// Returns the parsed thread root when so; `None` for every other shape (no
/// relation, a plain reply, or an explicit in-thread reply), which the caller
/// should route through the normal SDK attachment/reply path instead.
fn thread_member_root(relation: Relation<'_>) -> Result<Option<OwnedEventId>, GatewayError> {
    match (relation.thread_root, relation.reply_to) {
        (Some(root), None) => {
            Ok(Some(EventId::parse(root).map_err(|e| {
                GatewayError::Invalid(format!("thread_root: {e}"))
            })?))
        }
        _ => Ok(None),
    }
}

/// Upload `bytes` for a media send, matching the room's current encryption
/// state — encrypted upload for an encrypted room, plain upload otherwise —
/// mirroring what `Room::send_attachment` does internally (matrix-sdk 0.18
/// `room/mod.rs::prepare_and_send_attachment`), which isn't exposed for reuse.
async fn upload_media_source(
    room: &Room,
    content_type: &mime::Mime,
    bytes: Vec<u8>,
) -> Result<MediaSource, GatewayError> {
    let encrypted = room
        .latest_encryption_state()
        .await
        .map_err(map_sdk_err)?
        .is_encrypted();
    if encrypted {
        let mut cursor = std::io::Cursor::new(bytes);
        let file = room
            .client()
            .upload_encrypted_file(&mut cursor)
            .await
            .map_err(map_sdk_err)?;
        Ok(MediaSource::Encrypted(Box::new(file)))
    } else {
        let resp = room
            .client()
            .media()
            .upload(content_type, bytes, None)
            .await
            .map_err(map_sdk_err)?;
        Ok(MediaSource::Plain(resp.content_uri))
    }
}

/// Build the `m.image`/`m.file` [`MessageType`] for an already-uploaded
/// `source`, mirroring matrix-sdk's private `make_media_type!` for the two
/// kinds [`MediaSendKind`] supports. A caption becomes the event body (with
/// the original filename kept in `filename`); otherwise the filename is the
/// body and `filename` is left unset, matching `Room::send_attachment`'s
/// convention (MSC2530).
fn media_message_type(
    kind: MediaSendKind,
    filename: String,
    content_type: &mime::Mime,
    source: MediaSource,
    info: AttachmentInfo,
    caption: Option<&str>,
) -> MessageType {
    let (body, filename) = match caption {
        Some(caption) => (caption.to_owned(), Some(filename)),
        None => (filename, None),
    };
    let mimetype = Some(content_type.as_ref().to_owned());

    match kind {
        MediaSendKind::Image => {
            let mut info: ImageInfo = info.into();
            info.mimetype = mimetype;
            let mut content = ImageMessageEventContent::new(body, source);
            content.filename = filename;
            content.info = Some(Box::new(info));
            MessageType::Image(content)
        }
        MediaSendKind::File => {
            let mut info: FileInfo = info.into();
            info.mimetype = mimetype;
            let mut content = FileMessageEventContent::new(body, source);
            content.filename = filename;
            content.info = Some(Box::new(info));
            MessageType::File(content)
        }
    }
}

/// Send a media attachment as a thread member (`thread_root` without
/// `reply_to`): upload directly via [`upload_media_source`], then attach
/// `Thread::without_fallback` — `rel_type: m.thread` with no `m.in_reply_to` —
/// so it can't render as a reply in other clients (#266). See
/// [`SdkGateway::send_media`] for why this bypasses `room.send_attachment`.
async fn send_thread_member_attachment(
    room: &Room,
    attachment: MediaAttachment,
    content_type: &mime::Mime,
    info: AttachmentInfo,
    caption: Option<&str>,
    thread_root: OwnedEventId,
) -> Result<String, GatewayError> {
    let source = upload_media_source(room, content_type, attachment.bytes).await?;
    let msg_type = media_message_type(
        attachment.kind,
        attachment.filename,
        content_type,
        source,
        info,
        caption,
    );
    let mut content = RoomMessageEventContent::new(msg_type);
    content.relates_to = Some(MessageRelation::Thread(Thread::without_fallback(
        thread_root,
    )));
    let resp = room.send(content).await.map_err(map_sdk_err)?;
    Ok(resp.response.event_id.to_string())
}

fn message_relates_to(relation: Relation<'_>) -> Result<Option<Value>, GatewayError> {
    if let Some(id) = relation.reply_to {
        EventId::parse(id).map_err(|e| GatewayError::Invalid(format!("reply_to: {e}")))?;
    }
    if let Some(id) = relation.thread_root {
        EventId::parse(id).map_err(|e| GatewayError::Invalid(format!("thread_root: {e}")))?;
    }

    Ok(match (relation.thread_root, relation.reply_to) {
        // Thread member, not a reply. Do not invent an `m.in_reply_to` fallback:
        // without the latest known in-thread event, falling back to the root
        // makes thread additions render as replies to the first message.
        (Some(root), None) => Some(json!({
            "rel_type": "m.thread",
            "event_id": root,
        })),
        // Explicit reply within a thread.
        (Some(root), Some(reply)) => Some(json!({
            "rel_type": "m.thread",
            "event_id": root,
            "m.in_reply_to": {
                "event_id": reply,
            },
        })),
        // Plain reply (no thread).
        (None, Some(reply)) => Some(json!({ "m.in_reply_to": { "event_id": reply } })),
        (None, None) => None,
    })
}

/// Per-`(account_id, room_id)` lock guarding [`SdkGateway::set_power_levels`]'s
/// read-modify-send sequence (ADR 0068, M19e review). Without this, two
/// concurrent writers to the same room's power levels each read the same
/// pre-change snapshot via `room.power_levels()`, evaluate
/// [`check_self_demotion_guardrail`] against their own now-divergent
/// in-memory copy, and can both pass even though the combined effect strands
/// someone — whichever `send_state_event` lands second silently overwrites
/// the whole event (there is no per-field merge on the wire), losing the
/// earlier write too. Mirrors [`ClientManager`]'s per-account `Slot`
/// (`manager.rs`) and the lifecycle layer's `IdentityLock` (`lifecycle.rs`):
/// the outer [`Mutex`] is held only to fetch/insert a lock, the actual
/// critical section is held under the returned async mutex, so concurrent
/// writers to *different* rooms never block each other.
type PowerLevelLock = Arc<AsyncMutex<()>>;
type PowerLevelLocks = Arc<Mutex<HashMap<(Uuid, String), PowerLevelLock>>>;

/// Sends Matrix message-like events on behalf of an account, routed through that
/// account's SDK client. Cheap to [`Clone`] (every field is a handle).
#[derive(Clone)]
pub struct SdkGateway {
    manager: ClientManager,
    power_level_locks: PowerLevelLocks,
}

impl SdkGateway {
    /// Build a gateway over a client manager. Constructed by the sync engine and
    /// exposed via [`SyncEngine::gateway`](crate::SyncEngine::gateway).
    pub(crate) fn new(manager: ClientManager) -> Self {
        Self {
            manager,
            power_level_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Fetch (or create) the per-room power-level lock for `(account_id,
    /// room_id)`. See [`PowerLevelLocks`] for why this exists. The map grows
    /// unbounded — one entry per room ever written to — the same accepted
    /// tradeoff `lifecycle::lock_for`'s `IdentityLocks` makes: nothing here
    /// owns room-departure bookkeeping, so pruning isn't this gateway's job,
    /// and the leak is one small entry per distinct room.
    fn power_level_lock(&self, account_id: Uuid, room_id: &str) -> PowerLevelLock {
        let key = (account_id, room_id.to_owned());
        let mut map = self
            .power_level_locks
            .lock()
            .expect("power-level lock map poisoned");
        map.entry(key).or_default().clone()
    }

    /// Resolve the joined [`Room`] for `(account_id, room_id)`, connecting the
    /// account's client on demand. A malformed room id is a `400`-class
    /// [`GatewayError::Invalid`]; a room the client doesn't know is a `404`-class
    /// [`GatewayError::RoomNotFound`].
    async fn room(&self, account_id: Uuid, room_id: &str) -> Result<Room, GatewayError> {
        let client = self.manager.get_or_connect(account_id).await?;
        let parsed =
            RoomId::parse(room_id).map_err(|e| GatewayError::Invalid(format!("room id: {e}")))?;
        client
            .get_room(&parsed)
            .ok_or_else(|| GatewayError::RoomNotFound(room_id.to_owned()))
    }

    /// Send an `m.room.message`. `body` is the plain-text content; `formatted`,
    /// when present, adds the rich-text rendering (`format` + `formatted_body`).
    /// `relation`, when set, attaches an `m.relates_to` (a reply and/or thread
    /// membership). Returns the new event id.
    pub async fn send_message(
        &self,
        account_id: Uuid,
        room_id: &str,
        body: &str,
        formatted: Option<Formatted<'_>>,
        relation: Relation<'_>,
    ) -> Result<String, GatewayError> {
        let room = self.room(account_id, room_id).await?;

        // Plain, unrelated message: use the typed constructor. `format` is
        // validated to `org.matrix.custom.html` at the API boundary, so
        // `text_html` carries it faithfully.
        if !relation.is_some() {
            let content = match formatted {
                None => RoomMessageEventContent::text_plain(body),
                Some(f) => RoomMessageEventContent::text_html(body, f.body),
            };
            let resp = room.send(content).await.map_err(map_sdk_err)?;
            return Ok(resp.response.event_id.to_string());
        }

        // A relation is requested: build a raw envelope with `m.relates_to`,
        // mirroring how `edit` constructs its relation by hand. Validate the
        // referenced event ids up front so a bad id is a clean 400, not a 502.
        let relates_to = message_relates_to(relation)?.expect("relation.is_some() checked above");

        let mut content = json!({
            "msgtype": "m.text",
            "body": body,
            "m.relates_to": relates_to,
        });
        if let Some(f) = formatted {
            content["format"] = json!(f.format);
            content["formatted_body"] = json!(f.body);
        }
        let resp = room
            .send_raw("m.room.message", content)
            .await
            .map_err(map_sdk_err)?;
        Ok(resp.response.event_id.to_string())
    }

    /// Send a staged media attachment as an `m.image` or `m.file`.
    ///
    /// `thread_root` without `reply_to` (a thread member, not a reply) can't be
    /// expressed through the SDK's typed reply/attachment helper: `AttachmentConfig`
    /// only accepts a [`Reply`], and matrix-sdk 0.18's `make_for_thread`
    /// unconditionally sets a fallback `m.in_reply_to` even when no reply was
    /// requested (`room/reply.rs`; confirmed by the SDK's own
    /// `test_start_thread` unit test) — other clients render that fallback as
    /// a genuine reply to the thread root (#266). So that one relation shape
    /// is built by hand in [`send_thread_member_attachment`], driving the
    /// upload directly instead of through `room.send_attachment`. Every other
    /// shape (no relation, a plain reply, or an explicit in-thread reply)
    /// keeps going through the SDK helper below, which already gets those
    /// right.
    pub async fn send_media(
        &self,
        account_id: Uuid,
        room_id: &str,
        attachment: MediaAttachment,
        caption: Option<&str>,
        relation: Relation<'_>,
    ) -> Result<String, GatewayError> {
        let room = self.room(account_id, room_id).await?;
        let content_type = effective_mime(attachment.kind, attachment.content_type.as_deref())?;
        let info = attachment_info(attachment.kind, attachment.size_bytes)?;

        if let Some(thread_root) = thread_member_root(relation)? {
            return send_thread_member_attachment(
                &room,
                attachment,
                &content_type,
                info,
                caption,
                thread_root,
            )
            .await;
        }

        let reply = attachment_reply(relation)?;
        let caption = caption.map(TextMessageEventContent::plain);
        let config = AttachmentConfig::new()
            .info(info)
            .caption(caption)
            .reply(reply);
        let resp = room
            .send_attachment(attachment.filename, &content_type, attachment.bytes, config)
            .await
            .map_err(map_sdk_err)?;
        Ok(resp.event_id.to_string())
    }

    /// Edit a message by sending an `m.replace` replacement of `event_id`.
    /// Built as a raw envelope (`m.new_content` + `m.relates_to`) so we don't
    /// need the original event in hand. Returns the replacement event's id.
    pub async fn edit(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        body: &str,
        formatted: Option<Formatted<'_>>,
    ) -> Result<String, GatewayError> {
        let room = self.room(account_id, room_id).await?;
        // Validate the target id up front so a bad id is a clean 400, not a 502.
        let target_id = parse_event_id(event_id)?;

        // A Matrix edit (m.replace) is only valid from the *original author*, but
        // the homeserver does not enforce that — it accepts an m.replace pointing
        // at anyone's event. So we enforce it: fetch the target and refuse to send
        // a forged edit of a message this account didn't write (which would
        // otherwise return 200 and could be rendered by a naive client).
        let target = room.event(&target_id, None).await.map_err(map_sdk_err)?;
        if target.sender().as_deref() != Some(room.own_user_id()) {
            return Err(GatewayError::Forbidden(
                "can only edit your own messages".to_owned(),
            ));
        }

        let mut new_content = json!({ "msgtype": "m.text", "body": body });
        let mut content = json!({
            "msgtype": "m.text",
            // The fallback body convention for clients that don't understand edits.
            "body": format!("* {body}"),
            "m.relates_to": { "rel_type": "m.replace", "event_id": event_id },
        });
        if let Some(f) = formatted {
            // The replacement carries the formatting verbatim; the top-level
            // fallback mirrors it with the same `* ` edit prefix as `body`.
            new_content["format"] = json!(f.format);
            new_content["formatted_body"] = json!(f.body);
            content["format"] = json!(f.format);
            content["formatted_body"] = json!(format!("* {}", f.body));
        }
        content["m.new_content"] = new_content;
        let resp = room
            .send_raw("m.room.message", content)
            .await
            .map_err(map_sdk_err)?;
        Ok(resp.response.event_id.to_string())
    }

    /// Redact `event_id`, optionally with a reason. Returns the redaction event's id.
    pub async fn redact(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        reason: Option<&str>,
    ) -> Result<String, GatewayError> {
        let room = self.room(account_id, room_id).await?;
        let event_id = parse_event_id(event_id)?;
        let resp = room
            .redact(&event_id, reason, None)
            .await
            .map_err(|e| map_sdk_err(matrix_sdk::Error::from(e)))?;
        Ok(resp.event_id.to_string())
    }

    /// Send an `m.reaction` annotating `event_id` with `key` (an emoji or short
    /// string). Returns the reaction event's id.
    pub async fn react(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        key: &str,
    ) -> Result<String, GatewayError> {
        let room = self.room(account_id, room_id).await?;
        let event_id = parse_event_id(event_id)?;
        let content = ReactionEventContent::new(Annotation::new(event_id, key.to_owned()));
        let resp = room.send(content).await.map_err(map_sdk_err)?;
        Ok(resp.response.event_id.to_string())
    }

    /// Mark `event_id` read: sets the public read receipt (`m.read`) and the
    /// private fully-read marker to the same event in a single
    /// `POST /rooms/{roomId}/read_markers` call, so third-party Matrix clients
    /// reading standard receipt state see the room as read (ADR 0067).
    ///
    /// The caller's event id is sent **verbatim**. Choosing it is the client's
    /// job, because only the client knows which events it displayed: a receipt is
    /// interpreted in arrival order while every read surface sorts by
    /// `origin_ts`, so the client names the greatest `EventDto::arrival_order`
    /// among the events it actually showed (ADR 0089).
    ///
    /// An earlier version of this method resolved the target here, substituting
    /// the last event of the SDK's event-cache chunk when it looked like the
    /// client must have displayed it. That was unsound: a client pages by display
    /// order, so an event stamped older than its page floor is not in the page at
    /// all, and substituting it acknowledged messages that were never shown.
    /// Visibility is not derivable server-side, so it is not inferred here.
    pub async fn send_read_receipt(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
    ) -> Result<(), GatewayError> {
        let room = self.room(account_id, room_id).await?;
        let event_id = parse_event_id(event_id)?;
        let receipts = Receipts::new()
            .fully_read_marker(event_id.clone())
            .public_read_receipt(event_id);
        room.send_multiple_receipts(receipts)
            .await
            .map_err(map_sdk_err)
    }

    /// Set (or clear) this account's typing indicator in a room (ADR 0068,
    /// M19a). The SDK debounces/expires the underlying `m.typing` event itself;
    /// this issues one `PUT /rooms/{roomId}/typing/{userId}` per call.
    pub async fn send_typing_notice(
        &self,
        account_id: Uuid,
        room_id: &str,
        typing: bool,
    ) -> Result<(), GatewayError> {
        let room = self.room(account_id, room_id).await?;
        room.typing_notice(typing).await.map_err(map_sdk_err)
    }

    /// Leave this room (ADR 0068, M19b). The `m.room.member` leave event this
    /// produces already drives existing downstream handling — the ADR 0037
    /// membership filter and ADR 0044's opt-in `purge_on_leave` — via the sync
    /// path's own `persist_state_event`; this method only issues the call.
    pub async fn leave(&self, account_id: Uuid, room_id: &str) -> Result<(), GatewayError> {
        let room = self.room(account_id, room_id).await?;
        room.leave().await.map_err(map_sdk_err)
    }

    /// Forget a left or banned-from room (ADR 0068, M19b). The homeserver only
    /// accepts this for a room in `Left`/`Banned` state; a still-joined or
    /// -invited room is rejected up front as a clean 400 rather than round-
    /// tripping to the SDK for a `WrongRoomState` error that `map_sdk_err`
    /// can't distinguish from an upstream failure.
    pub async fn forget(&self, account_id: Uuid, room_id: &str) -> Result<(), GatewayError> {
        let room = self.room(account_id, room_id).await?;
        match room.state() {
            RoomState::Left | RoomState::Banned => {}
            other => {
                return Err(GatewayError::Invalid(format!(
                    "cannot forget a room in state {other:?}; only left or banned rooms can be forgotten"
                )))
            }
        }
        room.forget().await.map_err(map_sdk_err)
    }

    /// Invite `user_id` to this room (ADR 0068, M19b).
    pub async fn invite(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
    ) -> Result<(), GatewayError> {
        let room = self.room(account_id, room_id).await?;
        let user_id = parse_user_id(user_id)?;
        room.invite_user_by_id(&user_id).await.map_err(map_sdk_err)
    }

    /// Kick `user_id` from this room, optionally with a reason (ADR 0068, M19b).
    pub async fn kick(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), GatewayError> {
        let room = self.room(account_id, room_id).await?;
        let user_id = parse_user_id(user_id)?;
        room.kick_user(&user_id, reason).await.map_err(map_sdk_err)
    }

    /// Ban `user_id` from this room, optionally with a reason (ADR 0068, M19b).
    pub async fn ban(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), GatewayError> {
        let room = self.room(account_id, room_id).await?;
        let user_id = parse_user_id(user_id)?;
        room.ban_user(&user_id, reason).await.map_err(map_sdk_err)
    }

    /// Unban `user_id` from this room, optionally with a reason (ADR 0068, M19b).
    pub async fn unban(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), GatewayError> {
        let room = self.room(account_id, room_id).await?;
        let user_id = parse_user_id(user_id)?;
        room.unban_user(&user_id, reason).await.map_err(map_sdk_err)
    }

    /// Join a room by id or alias (ADR 0068, M19c). No `Room` handle exists
    /// yet, so this resolves via `ClientManager::get_or_connect` directly
    /// rather than `Self::room()`. `server_names` supplies federation-
    /// resolution hints for an alias this account's client has no direct path
    /// to. Returns the joined room's id.
    pub async fn join(
        &self,
        account_id: Uuid,
        room_id_or_alias: &str,
        server_names: &[String],
    ) -> Result<String, GatewayError> {
        let client = self.manager.get_or_connect(account_id).await?;
        let id = parse_room_id_or_alias(room_id_or_alias)?;
        let server_names = parse_server_names(server_names)?;
        let room = client
            .join_room_by_id_or_alias(&id, &server_names)
            .await
            .map_err(map_sdk_err)?;
        Ok(room.room_id().to_string())
    }

    /// Knock on a room, optionally with a reason (ADR 0068, M19c). Returns
    /// the id of the room knocked on.
    pub async fn knock(
        &self,
        account_id: Uuid,
        room_id_or_alias: &str,
        reason: Option<&str>,
        server_names: &[String],
    ) -> Result<String, GatewayError> {
        let client = self.manager.get_or_connect(account_id).await?;
        let id = parse_room_id_or_alias(room_id_or_alias)?;
        let server_names = parse_server_names(server_names)?;
        let room = client
            .knock(id, reason.map(ToOwned::to_owned), server_names)
            .await
            .map_err(map_sdk_err)?;
        Ok(room.room_id().to_string())
    }

    /// Create a new room (ADR 0068, M19c). See [`build_create_room_request`]
    /// for the `encrypted` → `initial_state` translation. Returns the created
    /// room's id.
    ///
    /// **Not idempotent.** Matrix's `createRoom` endpoint has no idempotency
    /// key, so a caller that retries this call after an
    /// `axon.sync.room_entry_timeout_secs` timeout — believing the first
    /// attempt failed, when it may have actually succeeded upstream after
    /// Axon's client-side future was dropped — will create a second, distinct
    /// room. This is an accepted, documented risk rather than a solved one;
    /// there is no Matrix-spec-level fix available at this layer.
    pub async fn create_room(
        &self,
        account_id: Uuid,
        request: CreateRoomRequest,
    ) -> Result<String, GatewayError> {
        let client = self.manager.get_or_connect(account_id).await?;
        let built = build_create_room_request(request)?;
        let is_direct = built.is_direct;
        let invite = built.invite.clone();
        let room = client.create_room(built).await.map_err(map_sdk_err)?;
        // `Client::create_room` already attempts `mark_as_dm` internally when
        // `is_direct` and `invite` are both set, but it only logs a failure
        // there rather than surfacing it — so double-check the outcome and
        // retry explicitly (propagating any error through our own structured
        // logging) if it didn't actually land.
        if is_direct && !invite.is_empty() && !room.is_direct().await.unwrap_or(false) {
            client
                .account()
                .mark_as_dm(room.room_id(), &invite)
                .await
                .map_err(map_sdk_err)?;
        }
        Ok(room.room_id().to_string())
    }

    /// Create a DM with `user_id` (ADR 0068, M19c). Delegates to
    /// the SDK's `Client::create_dm`, which — with the `e2e-encryption`
    /// feature this crate already enables — bundles `m.room.encryption` into
    /// `initial_state` and attempts `mark_as_dm` internally. That internal
    /// attempt only logs a failure rather than surfacing it, so this method
    /// double-checks the outcome and retries explicitly (propagating any
    /// error through our own structured logging) if it didn't land. Returns
    /// the DM room's id.
    ///
    /// Shares `create_room`'s non-idempotency risk: a client retry after an
    /// Axon-side timeout can create a second DM room upstream.
    pub async fn create_dm(&self, account_id: Uuid, user_id: &str) -> Result<String, GatewayError> {
        let client = self.manager.get_or_connect(account_id).await?;
        let user_id = parse_user_id(user_id)?;
        let room = client.create_dm(&user_id).await.map_err(map_sdk_err)?;
        if !room.is_direct().await.unwrap_or(false) {
            client
                .account()
                .mark_as_dm(room.room_id(), std::slice::from_ref(&user_id))
                .await
                .map_err(map_sdk_err)?;
        }
        Ok(room.room_id().to_string())
    }

    /// Set this room's `m.room.name` (ADR 0068, M19d). An empty `name` clears
    /// it — the SDK has no separate "remove" primitive for name/topic (unlike
    /// avatar), so a blank value is the clear signal.
    pub async fn set_name(
        &self,
        account_id: Uuid,
        room_id: &str,
        name: &str,
    ) -> Result<(), GatewayError> {
        let room = self.room(account_id, room_id).await?;
        ensure_joined(&room)?;
        room.set_name(name.to_owned()).await.map_err(map_sdk_err)?;
        Ok(())
    }

    /// Set this room's `m.room.topic` (ADR 0068, M19d). An empty `topic`
    /// clears it, same convention as [`set_name`](Self::set_name).
    pub async fn set_topic(
        &self,
        account_id: Uuid,
        room_id: &str,
        topic: &str,
    ) -> Result<(), GatewayError> {
        let room = self.room(account_id, room_id).await?;
        ensure_joined(&room)?;
        room.set_room_topic(topic).await.map_err(map_sdk_err)?;
        Ok(())
    }

    /// Upload `attachment`'s bytes as this room's avatar and set
    /// `m.room.avatar` to the resulting `mxc://` URI in one call
    /// (`Room::upload_avatar`, ADR 0068 M19d) — mirroring `send_media`'s
    /// single-call upload+send shape. The avatar must be an image regardless
    /// of the staged upload's own `kind` metadata, so this always validates
    /// against [`MediaSendKind::Image`] rather than trusting the caller's
    /// staged `kind` — and, unlike `send_media`, a missing `content_type` is
    /// rejected rather than defaulted to `image/png`: `effective_mime`'s
    /// "assume PNG when absent" fallback exists for message sends where any
    /// declared kind is acceptable, but here it would silently accept
    /// non-image bytes with no real validation at all.
    ///
    /// See [`ensure_joined`] for why the room-state precondition is checked
    /// explicitly here rather than left to the SDK.
    pub async fn set_avatar(
        &self,
        account_id: Uuid,
        room_id: &str,
        attachment: MediaAttachment,
    ) -> Result<(), GatewayError> {
        let room = self.room(account_id, room_id).await?;
        ensure_joined(&room)?;
        let content_type = attachment.content_type.as_deref().ok_or_else(|| {
            GatewayError::Invalid("avatar upload must declare a content type".to_owned())
        })?;
        let content_type = effective_mime(MediaSendKind::Image, Some(content_type))?;
        room.upload_avatar(&content_type, attachment.bytes, None)
            .await
            .map_err(map_sdk_err)?;
        Ok(())
    }

    /// Clear this room's `m.room.avatar` (ADR 0068, M19d). See
    /// [`ensure_joined`] for why the room-state precondition is checked
    /// explicitly here rather than left to the SDK.
    pub async fn remove_avatar(&self, account_id: Uuid, room_id: &str) -> Result<(), GatewayError> {
        let room = self.room(account_id, room_id).await?;
        ensure_joined(&room)?;
        room.remove_avatar().await.map_err(map_sdk_err)?;
        Ok(())
    }

    /// Add or update `tag` on this room, with an optional sort `order`
    /// (ADR 0068, M19d). Unlike `name`/`topic`/`avatar`, this writes **room
    /// account data** (`m.tag`, private to this account), not a state event —
    /// there is no `Room::state()`/`send_state_event` involved, only
    /// `Room::set_tag`'s direct `create_tag` call, which the homeserver
    /// accepts regardless of local join state, so this doesn't need
    /// [`ensure_joined`].
    pub async fn set_tag(
        &self,
        account_id: Uuid,
        room_id: &str,
        tag: &str,
        order: Option<f64>,
    ) -> Result<(), GatewayError> {
        let room = self.room(account_id, room_id).await?;
        let tag_name = parse_tag_name(tag)?;
        let order = validate_tag_order(order)?;
        let mut tag_info = TagInfo::new();
        tag_info.order = order;
        room.set_tag(tag_name, tag_info)
            .await
            .map_err(map_sdk_err)?;
        Ok(())
    }

    /// Remove `tag` from this room (ADR 0068, M19d). Same account-data
    /// caveat as [`set_tag`](Self::set_tag).
    pub async fn remove_tag(
        &self,
        account_id: Uuid,
        room_id: &str,
        tag: &str,
    ) -> Result<(), GatewayError> {
        let room = self.room(account_id, room_id).await?;
        let tag_name = parse_tag_name(tag)?;
        room.remove_tag(tag_name).await.map_err(map_sdk_err)?;
        Ok(())
    }

    /// Set this room's power levels — role thresholds (`ban`/`invite`/`kick`/
    /// `redact`/`events_default`/`state_default`/`users_default`) and
    /// individual per-user levels, merged into one `m.room.power_levels`
    /// state event (ADR 0068, M19e). See [`merge_power_level_changes`] for
    /// why this doesn't call matrix-sdk's own
    /// `apply_power_level_changes`/`update_power_levels` helpers directly.
    ///
    /// Before sending, [`check_self_demotion_guardrail`] verifies this
    /// request doesn't itself drop the caller's own resolved power level
    /// below what's needed to send another `m.room.power_levels` event. See
    /// [`ensure_joined`] for why the room-state precondition is checked
    /// explicitly here rather than left to the SDK.
    ///
    /// The whole read-merge-check-send sequence runs under a per-`(account_id,
    /// room_id)` lock (see [`PowerLevelLocks`]): without it, two concurrent
    /// calls for the same room could each read the same pre-change snapshot,
    /// pass the guardrail against their own now-stale copy, and have the
    /// second `send_state_event` silently clobber the first's changes —
    /// exactly the kind of shared-mutable-state race the guardrail exists to
    /// close, just from a second writer instead of this request's own two
    /// change axes (which [`merge_power_level_changes`]'s single-send design
    /// already closes).
    pub async fn set_power_levels(
        &self,
        account_id: Uuid,
        room_id: &str,
        changes: PowerLevelChanges,
    ) -> Result<(), GatewayError> {
        let room = self.room(account_id, room_id).await?;
        ensure_joined(&room)?;

        let lock = self.power_level_lock(account_id, room_id);
        let _guard = lock.lock().await;

        let before = room
            .power_levels()
            .await
            .map_err(|e| map_sdk_err(e.into()))?;
        let mut merged = before.clone();
        merge_power_level_changes(&mut merged, &changes)?;
        check_self_demotion_guardrail(
            &before,
            &merged,
            room.own_user_id(),
            changes.acknowledge_self_demotion,
        )?;

        let content = RoomPowerLevelsEventContent::try_from(merged)
            .map_err(|e| GatewayError::Invalid(format!("power levels: {e}")))?;
        room.send_state_event(content).await.map_err(map_sdk_err)?;
        Ok(())
    }

    /// Read this room's fully resolved power levels, defaults filled in
    /// (ADR 0068, M19e) — the same computation [`set_power_levels`]
    /// (Self::set_power_levels) performs internally for its guardrail,
    /// exposed as its own read so a client can build a power-levels UI. No
    /// generic Tier-2 state read exists yet to cover this (ADR 0055's
    /// proposal was never implemented), so without this there would be no
    /// way for a client to read a room's power levels through Axon at all.
    /// Shares [`set_power_levels`](Self::set_power_levels)'s [`ensure_joined`]
    /// guard so a left/banned/invited room's stale cached state can't be read
    /// through this route either, even though matrix-sdk's own `get_room`/
    /// `power_levels()` would otherwise happily return it.
    pub async fn power_levels(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<ResolvedPowerLevels, GatewayError> {
        let room = self.room(account_id, room_id).await?;
        ensure_joined(&room)?;
        let power_levels = room
            .power_levels()
            .await
            .map_err(|e| map_sdk_err(e.into()))?;
        Ok(ResolvedPowerLevels {
            ban: power_levels.ban.into(),
            invite: power_levels.invite.into(),
            kick: power_levels.kick.into(),
            redact: power_levels.redact.into(),
            events_default: power_levels.events_default.into(),
            state_default: power_levels.state_default.into(),
            users_default: power_levels.users_default.into(),
            users: power_levels
                .users
                .into_iter()
                .map(|(user_id, level)| (user_id.to_string(), level.into()))
                .collect(),
        })
    }

    /// Set this account's own display name (ADR 0068, M19f). An empty
    /// `display_name` clears it — the same "blank clears" convention as
    /// [`set_name`](Self::set_name) — issuing the SDK's real
    /// profile-field-delete call (`Account::set_display_name(None)`) rather
    /// than writing an empty string to the homeserver.
    pub async fn set_display_name(
        &self,
        account_id: Uuid,
        display_name: &str,
    ) -> Result<(), GatewayError> {
        let client = self.manager.get_or_connect(account_id).await?;
        let name = (!display_name.is_empty()).then_some(display_name);
        client
            .account()
            .set_display_name(name)
            .await
            .map_err(map_sdk_err)
    }

    /// Upload `attachment`'s bytes as this account's avatar and set the
    /// account's profile avatar to the resulting `mxc://` URI in one call
    /// (`Account::upload_avatar`, ADR 0068 M19f) — mirroring
    /// [`set_avatar`](Self::set_avatar)'s (room avatar) single-call
    /// upload+set shape and its same validation: the avatar must be an
    /// image regardless of the staged upload's own `kind` metadata, and a
    /// missing `content_type` is rejected rather than defaulted.
    pub async fn set_account_avatar(
        &self,
        account_id: Uuid,
        attachment: MediaAttachment,
    ) -> Result<(), GatewayError> {
        let client = self.manager.get_or_connect(account_id).await?;
        let content_type = attachment.content_type.as_deref().ok_or_else(|| {
            GatewayError::Invalid("avatar upload must declare a content type".to_owned())
        })?;
        let content_type = effective_mime(MediaSendKind::Image, Some(content_type))?;
        client
            .account()
            .upload_avatar(&content_type, attachment.bytes)
            .await
            .map_err(map_sdk_err)?;
        Ok(())
    }

    /// Clear this account's profile avatar (ADR 0068, M19f).
    pub async fn remove_account_avatar(&self, account_id: Uuid) -> Result<(), GatewayError> {
        let client = self.manager.get_or_connect(account_id).await?;
        client
            .account()
            .set_avatar_url(None)
            .await
            .map_err(map_sdk_err)
    }

    /// Read `user_id`'s Matrix profile (ADR 0068, M19f). `user_id` may be
    /// this account's own user id or any other user's; either returned field
    /// may be absent for a user who never set it, not an error.
    pub async fn fetch_user_profile_of(
        &self,
        account_id: Uuid,
        user_id: &str,
    ) -> Result<MatrixProfile, GatewayError> {
        let client = self.manager.get_or_connect(account_id).await?;
        let user_id = parse_user_id(user_id)?;
        let response = client
            .account()
            .fetch_user_profile_of(&user_id)
            .await
            .map_err(map_sdk_err)?;
        let display_name = response
            .get_static::<DisplayName>()
            .map_err(|e| GatewayError::Upstream(format!("profile display_name: {e}")))?;
        let avatar_url = response
            .get_static::<AvatarUrl>()
            .map_err(|e| GatewayError::Upstream(format!("profile avatar_url: {e}")))?
            .map(|url| url.to_string());
        Ok(MatrixProfile {
            display_name,
            avatar_url,
        })
    }

    /// Add `user_id` to this account's ignore list (ADR 0068, M19f). Rejects
    /// an attempt to ignore this account's own user id up front as a clean
    /// `400` — the SDK's own guard (`Error::CantIgnoreLoggedInUser`) is a
    /// local precondition failure, not a homeserver response, so it carries
    /// no `client_api_error_kind` and `map_sdk_err`'s catch-all would
    /// otherwise surface it as a misleading `502`.
    pub async fn ignore_user(&self, account_id: Uuid, user_id: &str) -> Result<(), GatewayError> {
        let client = self.manager.get_or_connect(account_id).await?;
        let user_id = parse_user_id(user_id)?;
        if Some(user_id.as_ref()) == client.user_id() {
            return Err(GatewayError::Invalid(
                "cannot ignore this account's own user id".to_owned(),
            ));
        }
        client
            .account()
            .ignore_user(&user_id)
            .await
            .map_err(map_sdk_err)
    }

    /// Remove `user_id` from this account's ignore list (ADR 0068, M19f).
    pub async fn unignore_user(&self, account_id: Uuid, user_id: &str) -> Result<(), GatewayError> {
        let client = self.manager.get_or_connect(account_id).await?;
        let user_id = parse_user_id(user_id)?;
        client
            .account()
            .unignore_user(&user_id)
            .await
            .map_err(map_sdk_err)
    }

    /// Search a homeserver's public-room directory (`public_rooms_filtered`,
    /// ADR 0068 M19f). Unlike every other M19f method, this is a paginated
    /// **read**: `query.server`, when set, searches that server's directory
    /// instead of the account's own homeserver's.
    pub async fn public_rooms(
        &self,
        account_id: Uuid,
        query: PublicRoomsQuery,
    ) -> Result<PublicRoomsPage, GatewayError> {
        let client = self.manager.get_or_connect(account_id).await?;
        let server = query.server.as_deref().map(parse_server_name).transpose()?;

        let mut request = get_public_rooms_filtered::v3::Request::new();
        request.server = server;
        request.limit = query.limit.map(UInt::from);
        request.since = query.since;
        let mut filter = Filter::new();
        filter.generic_search_term = query.search_term;
        request.filter = filter;

        let response = client
            .public_rooms_filtered(request)
            .await
            .map_err(|e| map_sdk_err(matrix_sdk::Error::from(e)))?;

        Ok(PublicRoomsPage {
            chunk: response
                .chunk
                .into_iter()
                .map(|chunk| PublicRoomSummary {
                    room_id: chunk.room_id.to_string(),
                    canonical_alias: chunk.canonical_alias.map(|a| a.to_string()),
                    name: chunk.name,
                    topic: chunk.topic,
                    avatar_url: chunk.avatar_url.map(|u| u.to_string()),
                    num_joined_members: chunk.num_joined_members.into(),
                    world_readable: chunk.world_readable,
                    guest_can_join: chunk.guest_can_join,
                    join_rule: chunk.join_rule.as_str().to_owned(),
                    room_type: chunk.room_type.map(|t| t.as_str().to_owned()),
                })
                .collect(),
            next_batch: response.next_batch,
            prev_batch: response.prev_batch,
            total_room_count_estimate: response.total_room_count_estimate.map(u64::from),
        })
    }
}

#[cfg(test)]
mod tests {
    use axon_core::{CreateRoomRequest, MediaSendKind, PowerLevelChanges, Relation, RoomPreset};
    use matrix_sdk::attachment::{AttachmentInfo, BaseFileInfo, BaseImageInfo};
    use matrix_sdk::room::reply::EnforceThread;
    use matrix_sdk::ruma::api::client::room::Visibility;
    use matrix_sdk::ruma::events::room::message::{AddMentions, MessageType, ReplyWithinThread};
    use matrix_sdk::ruma::events::room::power_levels::{RoomPowerLevels, RoomPowerLevelsSource};
    use matrix_sdk::ruma::events::room::MediaSource;
    use matrix_sdk::ruma::events::tag::TagName;
    use matrix_sdk::ruma::{Int, OwnedMxcUri, RoomVersionId, UserId};
    use serde_json::json;

    use super::{
        attachment_info, attachment_reply, build_create_room_request,
        check_self_demotion_guardrail, effective_mime, media_message_type,
        merge_power_level_changes, message_relates_to, parse_room_id_or_alias, parse_server_name,
        parse_server_names, parse_tag_name, parse_user_id, thread_member_root, validate_tag_order,
        MAX_TAG_NAME_BYTES,
    };
    use crate::error::GatewayError;

    /// A room-version-11 [`RoomPowerLevels`] with no explicit event content
    /// (spec defaults: `state_default`/`ban`/`kick`/`redact` = 50,
    /// `events_default`/`invite`/`users_default` = 0, no `users` entries),
    /// for the [`merge_power_level_changes`]/[`check_self_demotion_guardrail`]
    /// unit tests below — these are pure functions over [`RoomPowerLevels`],
    /// so no live `Room`/homeserver is needed to exercise them.
    fn default_power_levels() -> RoomPowerLevels {
        let rules = RoomVersionId::V11
            .rules()
            .expect("V11 has rules")
            .authorization;
        RoomPowerLevels::new(RoomPowerLevelsSource::None, &rules, std::iter::empty())
    }

    const REPLY_EVENT: &str = "$reply:example.org";
    const THREAD_ROOT: &str = "$thread:example.org";

    #[test]
    fn parse_user_id_accepts_a_valid_id() {
        let user_id = parse_user_id("@alice:example.org").expect("valid user id");
        assert_eq!(user_id.as_str(), "@alice:example.org");
    }

    #[test]
    fn parse_user_id_rejects_a_malformed_id() {
        let err = parse_user_id("not-a-user-id").expect_err("malformed user id should fail");
        assert!(matches!(err, GatewayError::Invalid(message) if message.starts_with("user id:")));
    }

    #[test]
    fn parse_room_id_or_alias_accepts_a_room_id() {
        let id = parse_room_id_or_alias("!room:example.org").expect("valid room id");
        assert_eq!(id.as_str(), "!room:example.org");
    }

    #[test]
    fn parse_room_id_or_alias_accepts_an_alias() {
        let id = parse_room_id_or_alias("#room:example.org").expect("valid room alias");
        assert_eq!(id.as_str(), "#room:example.org");
    }

    #[test]
    fn parse_room_id_or_alias_rejects_a_malformed_id() {
        let err = parse_room_id_or_alias("not-a-room-id").expect_err("malformed id should fail");
        assert!(
            matches!(err, GatewayError::Invalid(message) if message.starts_with("room id or alias:"))
        );
    }

    #[test]
    fn parse_server_name_accepts_a_valid_name() {
        let name = parse_server_name("example.org").expect("valid server name");
        assert_eq!(name.as_str(), "example.org");
    }

    #[test]
    fn parse_server_name_rejects_a_malformed_name() {
        let err = parse_server_name("not a server name").expect_err("malformed name should fail");
        assert!(matches!(err, GatewayError::Invalid(message) if message.starts_with("server:")));
    }

    #[test]
    fn parse_server_names_accepts_a_valid_list() {
        let names = parse_server_names(&["example.org".to_owned(), "matrix.org".to_owned()])
            .expect("valid server names");
        assert_eq!(names.len(), 2);
        assert_eq!(names[0].as_str(), "example.org");
    }

    #[test]
    fn parse_server_names_rejects_one_bad_entry() {
        let err = parse_server_names(&["example.org".to_owned(), "not a server name".to_owned()])
            .expect_err("a malformed entry should fail the whole list");
        assert!(
            matches!(err, GatewayError::Invalid(message) if message.starts_with("server name"))
        );
    }

    #[test]
    fn parse_tag_name_accepts_well_known_names() {
        assert_eq!(parse_tag_name("m.favourite").unwrap(), TagName::Favorite);
        assert_eq!(
            parse_tag_name("m.lowpriority").unwrap(),
            TagName::LowPriority
        );
        assert_eq!(
            parse_tag_name("m.server_notice").unwrap(),
            TagName::ServerNotice
        );
    }

    #[test]
    fn parse_tag_name_accepts_a_custom_user_tag() {
        let tag = parse_tag_name("u.work").expect("valid custom tag");
        assert_eq!(tag.as_ref(), "u.work");
    }

    #[test]
    fn parse_tag_name_rejects_a_bare_u_prefix() {
        let err = parse_tag_name("u.").expect_err("empty custom tag name should fail");
        assert!(matches!(err, GatewayError::Invalid(message) if message.starts_with("tag:")));
    }

    #[test]
    fn parse_tag_name_rejects_an_unrecognized_string() {
        let err = parse_tag_name("not-a-tag").expect_err("unrecognized tag should fail");
        assert!(matches!(err, GatewayError::Invalid(message) if message.starts_with("tag:")));
    }

    #[test]
    fn parse_tag_name_accepts_a_tag_at_the_byte_limit() {
        let tag = format!("u.{}", "a".repeat(MAX_TAG_NAME_BYTES - 2));
        assert_eq!(tag.len(), MAX_TAG_NAME_BYTES);
        parse_tag_name(&tag).expect("tag at the byte limit should be accepted");
    }

    #[test]
    fn parse_tag_name_rejects_a_tag_over_the_byte_limit() {
        let tag = format!("u.{}", "a".repeat(MAX_TAG_NAME_BYTES - 1));
        assert_eq!(tag.len(), MAX_TAG_NAME_BYTES + 1);
        let err = parse_tag_name(&tag).expect_err("oversized tag should fail");
        assert!(matches!(err, GatewayError::Invalid(message) if message.starts_with("tag:")));
    }

    #[test]
    fn validate_tag_order_accepts_the_documented_range() {
        assert_eq!(validate_tag_order(None).unwrap(), None);
        assert_eq!(validate_tag_order(Some(0.0)).unwrap(), Some(0.0));
        assert_eq!(validate_tag_order(Some(1.0)).unwrap(), Some(1.0));
        assert_eq!(validate_tag_order(Some(0.5)).unwrap(), Some(0.5));
    }

    #[test]
    fn validate_tag_order_rejects_out_of_range_values() {
        for value in [-0.001, 1.001, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let err = validate_tag_order(Some(value)).expect_err("out-of-range order should fail");
            assert!(
                matches!(err, GatewayError::Invalid(message) if message.starts_with("tag order"))
            );
        }
    }

    #[test]
    fn build_create_room_request_maps_fields() {
        let request = CreateRoomRequest {
            name: Some("Room".to_owned()),
            topic: Some("Topic".to_owned()),
            invite: vec!["@alice:example.org".to_owned()],
            is_direct: true,
            public: true,
            preset: Some(RoomPreset::TrustedPrivate),
            encrypted: false,
        };
        let built = build_create_room_request(request).expect("valid request");
        assert_eq!(built.name.as_deref(), Some("Room"));
        assert_eq!(built.topic.as_deref(), Some("Topic"));
        assert_eq!(built.invite.len(), 1);
        assert!(built.is_direct);
        assert_eq!(built.visibility, Visibility::Public);
        assert_eq!(
            built.preset,
            Some(matrix_sdk::ruma::api::client::room::create_room::v3::RoomPreset::TrustedPrivateChat)
        );
    }

    #[test]
    fn build_create_room_request_encrypted_true_adds_encryption_initial_state() {
        let request = CreateRoomRequest {
            encrypted: true,
            ..Default::default()
        };
        let built = build_create_room_request(request).expect("valid request");
        assert_eq!(built.initial_state.len(), 1);
        let event_type: Option<String> = built.initial_state[0]
            .get_field("type")
            .expect("event has a type field");
        assert_eq!(event_type.as_deref(), Some("m.room.encryption"));
    }

    #[test]
    fn build_create_room_request_encrypted_false_has_empty_initial_state() {
        let request = CreateRoomRequest {
            encrypted: false,
            ..Default::default()
        };
        let built = build_create_room_request(request).expect("valid request");
        assert!(built.initial_state.is_empty());
    }

    #[test]
    fn build_create_room_request_rejects_bad_invite_user_id() {
        let request = CreateRoomRequest {
            invite: vec!["not-a-user-id".to_owned()],
            ..Default::default()
        };
        let err = build_create_room_request(request).expect_err("bad invite id should fail");
        assert!(matches!(err, GatewayError::Invalid(message) if message.starts_with("user id:")));
    }

    #[test]
    fn effective_mime_uses_kind_defaults() {
        assert_eq!(
            effective_mime(MediaSendKind::Image, None).expect("image default"),
            mime::IMAGE_PNG
        );
        assert_eq!(
            effective_mime(MediaSendKind::File, None).expect("file default"),
            mime::APPLICATION_OCTET_STREAM
        );
    }

    #[test]
    fn effective_mime_accepts_matching_image_type() {
        assert_eq!(
            effective_mime(MediaSendKind::Image, Some("image/jpeg")).expect("image jpeg"),
            mime::IMAGE_JPEG
        );
    }

    #[test]
    fn effective_mime_rejects_non_image_type_for_image_send() {
        let err = effective_mime(MediaSendKind::Image, Some("application/pdf"))
            .expect_err("non-image content type should fail");

        assert!(
            matches!(err, GatewayError::Invalid(message) if message == "image uploads must have an image/* content type")
        );
    }

    #[test]
    fn effective_mime_coerces_media_types_for_file_send() {
        for content_type in ["image/png", "audio/ogg", "video/mp4"] {
            assert_eq!(
                effective_mime(MediaSendKind::File, Some(content_type)).expect("file mime"),
                mime::APPLICATION_OCTET_STREAM
            );
        }
    }

    #[test]
    fn effective_mime_rejects_unparseable_content_type() {
        let err = effective_mime(MediaSendKind::File, Some("not a mime type"))
            .expect_err("invalid content type should fail");

        assert!(
            matches!(err, GatewayError::Invalid(message) if message.starts_with("content_type:"))
        );
    }

    #[test]
    fn attachment_info_records_size_for_each_send_kind() {
        let image_info = attachment_info(MediaSendKind::Image, 123).expect("image info");
        let AttachmentInfo::Image(image_info) = image_info else {
            panic!("expected image attachment info");
        };
        assert_eq!(u64::from(image_info.size.expect("image size")), 123);

        let file_info = attachment_info(MediaSendKind::File, 456).expect("file info");
        let AttachmentInfo::File(file_info) = file_info else {
            panic!("expected file attachment info");
        };
        assert_eq!(u64::from(file_info.size.expect("file size")), 456);
    }

    #[test]
    fn attachment_info_rejects_sizes_too_large_for_matrix_metadata() {
        let err = attachment_info(MediaSendKind::File, u64::MAX)
            .expect_err("oversized attachment metadata should fail");

        assert!(
            matches!(err, GatewayError::Invalid(message) if message == "upload is too large for Matrix metadata")
        );
    }

    fn plain_media_source() -> MediaSource {
        MediaSource::Plain(OwnedMxcUri::from("mxc://example.org/abc"))
    }

    #[test]
    fn media_message_type_builds_image_without_caption() {
        let content_type: mime::Mime = "image/png".parse().expect("valid mime");
        let info = AttachmentInfo::Image(BaseImageInfo {
            size: Some(123u32.into()),
            ..BaseImageInfo::default()
        });
        let msg_type = media_message_type(
            MediaSendKind::Image,
            "cat.png".to_owned(),
            &content_type,
            plain_media_source(),
            info,
            None,
        );
        let MessageType::Image(content) = msg_type else {
            panic!("expected an image message type");
        };
        // No caption: the filename is the body, and `filename` is left unset
        // (MSC2530) — a set `filename` alongside an identical `body` would
        // make clients treat the filename as a user-written caption.
        assert_eq!(content.body, "cat.png");
        assert_eq!(content.filename, None);
        let info = content.info.expect("image info");
        assert_eq!(info.mimetype.as_deref(), Some("image/png"));
        assert_eq!(u64::from(info.size.expect("image size")), 123);
    }

    #[test]
    fn media_message_type_builds_file_with_caption() {
        let content_type: mime::Mime = "application/pdf".parse().expect("valid mime");
        let info = AttachmentInfo::File(BaseFileInfo {
            size: Some(456u32.into()),
        });
        let msg_type = media_message_type(
            MediaSendKind::File,
            "report.pdf".to_owned(),
            &content_type,
            plain_media_source(),
            info,
            Some("quarterly report"),
        );
        let MessageType::File(content) = msg_type else {
            panic!("expected a file message type");
        };
        // A caption becomes the body, with the original filename preserved
        // separately (MSC2530) — otherwise the caption text would be lost.
        assert_eq!(content.body, "quarterly report");
        assert_eq!(content.filename.as_deref(), Some("report.pdf"));
        let info = content.info.expect("file info");
        assert_eq!(info.mimetype.as_deref(), Some("application/pdf"));
        assert_eq!(u64::from(info.size.expect("file size")), 456);
    }

    #[test]
    fn message_relates_to_maps_thread_member_without_reply() {
        let relates_to = message_relates_to(Relation {
            reply_to: None,
            thread_root: Some(THREAD_ROOT),
        })
        .expect("valid relation")
        .expect("thread relation");

        assert_eq!(
            relates_to,
            json!({
                "rel_type": "m.thread",
                "event_id": THREAD_ROOT,
            })
        );
    }

    #[test]
    fn message_relates_to_maps_thread_reply() {
        let relates_to = message_relates_to(Relation {
            reply_to: Some(REPLY_EVENT),
            thread_root: Some(THREAD_ROOT),
        })
        .expect("valid relation")
        .expect("thread reply relation");

        assert_eq!(
            relates_to,
            json!({
                "rel_type": "m.thread",
                "event_id": THREAD_ROOT,
                "m.in_reply_to": {
                    "event_id": REPLY_EVENT,
                },
            })
        );
    }

    #[test]
    fn message_relates_to_maps_plain_reply() {
        let relates_to = message_relates_to(Relation {
            reply_to: Some(REPLY_EVENT),
            thread_root: None,
        })
        .expect("valid relation")
        .expect("reply relation");

        assert_eq!(
            relates_to,
            json!({ "m.in_reply_to": { "event_id": REPLY_EVENT } })
        );
    }

    #[test]
    fn message_relates_to_rejects_invalid_event_ids() {
        let bad_reply = message_relates_to(Relation {
            reply_to: Some("not-an-event-id"),
            thread_root: None,
        })
        .expect_err("bad reply id should fail");
        assert!(
            matches!(bad_reply, GatewayError::Invalid(message) if message.starts_with("reply_to:"))
        );

        let bad_thread = message_relates_to(Relation {
            reply_to: None,
            thread_root: Some("not-an-event-id"),
        })
        .expect_err("bad thread id should fail");
        assert!(
            matches!(bad_thread, GatewayError::Invalid(message) if message.starts_with("thread_root:"))
        );
    }

    #[test]
    fn attachment_reply_returns_none_without_relation() {
        assert!(attachment_reply(Relation::default())
            .expect("unrelated attachment")
            .is_none());
    }

    #[test]
    fn attachment_reply_maps_plain_reply() {
        let reply = attachment_reply(Relation {
            reply_to: Some(REPLY_EVENT),
            thread_root: None,
        })
        .expect("plain reply")
        .expect("reply metadata");

        assert_eq!(reply.event_id.as_str(), REPLY_EVENT);
        assert_eq!(reply.enforce_thread, EnforceThread::Unthreaded);
        assert_eq!(reply.add_mentions, AddMentions::Yes);
    }

    #[test]
    fn attachment_reply_returns_none_for_thread_member_only() {
        // thread_root without reply_to is the "thread member, not a reply"
        // shape (#266) — that's `thread_member_root`'s job, not this
        // function's, precisely because this function has no way to express
        // it without a fallback `m.in_reply_to`.
        assert!(attachment_reply(Relation {
            reply_to: None,
            thread_root: Some(THREAD_ROOT),
        })
        .expect("thread_root-only is not an error")
        .is_none());
    }

    #[test]
    fn attachment_reply_maps_thread_with_explicit_reply() {
        let reply = attachment_reply(Relation {
            reply_to: Some(REPLY_EVENT),
            thread_root: Some(THREAD_ROOT),
        })
        .expect("thread reply relation")
        .expect("reply metadata");

        assert_eq!(reply.event_id.as_str(), REPLY_EVENT);
        assert_eq!(
            reply.enforce_thread,
            EnforceThread::Threaded(ReplyWithinThread::Yes)
        );
        assert_eq!(reply.add_mentions, AddMentions::Yes);
    }

    #[test]
    fn attachment_reply_rejects_invalid_event_ids() {
        let bad_reply = attachment_reply(Relation {
            reply_to: Some("not-an-event-id"),
            thread_root: None,
        })
        .expect_err("bad reply id should fail");
        assert!(
            matches!(bad_reply, GatewayError::Invalid(message) if message.starts_with("reply_to:"))
        );

        let bad_thread = attachment_reply(Relation {
            reply_to: Some(REPLY_EVENT),
            thread_root: Some("not-an-event-id"),
        })
        .expect_err("bad thread id should fail");
        assert!(
            matches!(bad_thread, GatewayError::Invalid(message) if message.starts_with("thread_root:"))
        );
    }

    #[test]
    fn thread_member_root_returns_none_without_thread_root() {
        assert!(thread_member_root(Relation::default())
            .expect("unrelated attachment")
            .is_none());
        assert!(thread_member_root(Relation {
            reply_to: Some(REPLY_EVENT),
            thread_root: None,
        })
        .expect("plain reply")
        .is_none());
    }

    #[test]
    fn thread_member_root_returns_none_when_reply_to_is_also_set() {
        // thread_root + reply_to is an explicit in-thread reply, which
        // `attachment_reply` already handles correctly — not this function's
        // job.
        assert!(thread_member_root(Relation {
            reply_to: Some(REPLY_EVENT),
            thread_root: Some(THREAD_ROOT),
        })
        .expect("thread reply")
        .is_none());
    }

    #[test]
    fn thread_member_root_parses_thread_root_only() {
        let root = thread_member_root(Relation {
            reply_to: None,
            thread_root: Some(THREAD_ROOT),
        })
        .expect("thread member")
        .expect("thread root present");
        assert_eq!(root.as_str(), THREAD_ROOT);
    }

    #[test]
    fn thread_member_root_rejects_invalid_event_id() {
        let err = thread_member_root(Relation {
            reply_to: None,
            thread_root: Some("not-an-event-id"),
        })
        .expect_err("bad thread id should fail");
        assert!(
            matches!(err, GatewayError::Invalid(message) if message.starts_with("thread_root:"))
        );
    }

    #[test]
    fn merge_power_level_changes_applies_role_thresholds() {
        let mut power_levels = default_power_levels();
        let changes = PowerLevelChanges {
            ban: Some(80),
            state_default: Some(60),
            ..Default::default()
        };
        merge_power_level_changes(&mut power_levels, &changes).expect("valid change");
        assert_eq!(power_levels.ban, Int::from(80));
        assert_eq!(power_levels.state_default, Int::from(60));
        // Untouched fields keep their spec default.
        assert_eq!(power_levels.kick, Int::from(50));
        assert_eq!(power_levels.users_default, Int::from(0));
    }

    #[test]
    fn merge_power_level_changes_merges_user_levels_without_clobbering_others() {
        let mut power_levels = default_power_levels();
        let alice = UserId::parse("@alice:example.org").unwrap();
        power_levels.users.insert(alice.clone(), 50.into());

        let mut users = std::collections::HashMap::new();
        users.insert("@bob:example.org".to_owned(), 75);
        let changes = PowerLevelChanges {
            users,
            ..Default::default()
        };
        merge_power_level_changes(&mut power_levels, &changes).expect("valid change");

        assert_eq!(power_levels.users.get(&alice).copied(), Some(50.into()));
        let bob = UserId::parse("@bob:example.org").unwrap();
        assert_eq!(power_levels.users.get(&bob).copied(), Some(75.into()));
    }

    #[test]
    fn merge_power_level_changes_removes_a_user_entry_set_back_to_the_default() {
        let mut power_levels = default_power_levels();
        let bob = UserId::parse("@bob:example.org").unwrap();
        power_levels.users.insert(bob.clone(), 50.into());

        let mut users = std::collections::HashMap::new();
        users.insert("@bob:example.org".to_owned(), 0); // users_default is 0
        let changes = PowerLevelChanges {
            users,
            ..Default::default()
        };
        merge_power_level_changes(&mut power_levels, &changes).expect("valid change");

        assert!(
            !power_levels.users.contains_key(&bob),
            "a level equal to users_default should be removed, not written as an explicit override"
        );
    }

    #[test]
    fn merge_power_level_changes_rejects_a_malformed_user_id() {
        let mut power_levels = default_power_levels();
        let mut users = std::collections::HashMap::new();
        users.insert("not-a-user-id".to_owned(), 50);
        let changes = PowerLevelChanges {
            users,
            ..Default::default()
        };
        let err = merge_power_level_changes(&mut power_levels, &changes).expect_err("bad user id");
        assert!(matches!(err, GatewayError::Invalid(message) if message.starts_with("user id:")));
    }

    #[test]
    fn check_self_demotion_guardrail_allows_a_no_op_when_caller_already_lacks_permission() {
        let before = default_power_levels();
        let own_user_id = UserId::parse("@alice:example.org").unwrap();
        // Caller is already at the default (0), below state_default (50) —
        // nothing about this request caused that (it's a no-op), so the
        // guardrail must not block it: doing so would misleadingly imply
        // acknowledge_self_demotion could rescue a caller the homeserver is
        // going to reject with 403 regardless.
        let merged = before.clone();

        check_self_demotion_guardrail(&before, &merged, &own_user_id, false)
            .expect("a request that doesn't itself strand the caller must not be blocked");
    }

    #[test]
    fn check_self_demotion_guardrail_blocks_a_change_that_causes_a_self_lockout() {
        let mut before = default_power_levels();
        let own_user_id = UserId::parse("@alice:example.org").unwrap();
        // Caller currently holds an explicit level (100) at/above
        // state_default (50), so they can self-correct today.
        before.users.insert(own_user_id.clone(), 100.into());

        let mut merged = before.clone();
        let mut users = std::collections::HashMap::new();
        users.insert(own_user_id.to_string(), 10);
        let changes = PowerLevelChanges {
            users,
            ..Default::default()
        };
        merge_power_level_changes(&mut merged, &changes).unwrap();

        let err = check_self_demotion_guardrail(&before, &merged, &own_user_id, false).expect_err(
            "this request itself drops the caller below what's required to self-correct",
        );
        assert!(matches!(err, GatewayError::Invalid(_)));
    }

    #[test]
    fn check_self_demotion_guardrail_allows_the_bypass_flag() {
        let mut before = default_power_levels();
        let own_user_id = UserId::parse("@alice:example.org").unwrap();
        before.users.insert(own_user_id.clone(), 100.into());
        let mut merged = before.clone();
        let mut users = std::collections::HashMap::new();
        users.insert(own_user_id.to_string(), 10);
        let changes = PowerLevelChanges {
            users,
            ..Default::default()
        };
        merge_power_level_changes(&mut merged, &changes).unwrap();

        check_self_demotion_guardrail(&before, &merged, &own_user_id, true)
            .expect("acknowledge_self_demotion bypasses the guardrail");
    }

    #[test]
    fn check_self_demotion_guardrail_allows_a_change_that_keeps_caller_at_required_level() {
        let mut before = default_power_levels();
        let own_user_id = UserId::parse("@alice:example.org").unwrap();
        before.users.insert(own_user_id.clone(), 100.into());
        let mut merged = before.clone();
        let mut users = std::collections::HashMap::new();
        users.insert(own_user_id.to_string(), 100);
        let changes = PowerLevelChanges {
            users,
            ..Default::default()
        };
        merge_power_level_changes(&mut merged, &changes).unwrap();

        check_self_demotion_guardrail(&before, &merged, &own_user_id, false)
            .expect("caller's own level (100) stays at/above state_default (50)");
    }
}
