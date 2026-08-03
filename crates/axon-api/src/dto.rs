//! Wire DTOs for the read API, mapped from `axon-store` row types.
//!
//! The store rows (`RoomSummary`, `TimelineRow`) are store-internal and don't
//! derive `Serialize`; these are the public JSON shapes, owned by the API layer.

use std::collections::{BTreeMap, HashMap};

use axon_core::{
    CreateRoomRequest, MatrixProfile, PowerLevelChanges, PublicRoomSummary, PublicRoomsPage,
    ResolvedPowerLevels, RoomPreset,
};
use axon_store::{
    Account, AccountState, DeviceStateRow, ReactionTally, RoomSummary, SpaceChildRow,
    SpaceParentRow, ThreadSummary, TimelineRow,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// A room in the cross-account list (`GET /v1/rooms`). Identity is
/// `(account_id, room_id)` — a room joined by two accounts appears twice.
#[derive(Debug, Serialize, ToSchema)]
pub struct RoomDto {
    /// Axon account this room belongs to.
    pub account_id: Uuid,
    /// Matrix user ID for this Axon account.
    pub account_user_id: String,
    /// Matrix room ID.
    pub room_id: String,
    /// Room name (`m.room.name`), if set.
    pub name: Option<String>,
    /// Room topic (`m.room.topic`), if set.
    pub topic: Option<String>,
    /// Avatar `mxc://` URI (`m.room.avatar`), if set.
    pub avatar_url: Option<String>,
    /// Canonical alias (`m.room.canonical_alias`), if set.
    pub canonical_alias: Option<String>,
    /// The room's `m.room.create` `type`, if any (for example `m.space`).
    pub room_type: Option<String>,
    /// `origin_server_ts` of the most recent event, in milliseconds — the sort key.
    pub last_activity_ts: i64,
    /// The most recent event's id, if the room has any events.
    pub last_event_id: Option<String>,
    /// SDK-derived unread notification count (issue #313, ADR 0070). This is
    /// matrix-sdk's client-side read-receipt-derived
    /// `Room::num_unread_notifications()` value, cached by Axon so a fresh
    /// client load can show a real number without first observing a live event
    /// this session. `0` until the sync engine has captured a value for this
    /// room.
    pub notification_count: i64,
    /// SDK-derived highlight count (issue #313, ADR 0070), from
    /// `Room::num_unread_mentions()`.
    pub highlight_count: i64,
}

impl From<RoomSummary> for RoomDto {
    fn from(r: RoomSummary) -> Self {
        RoomDto {
            account_id: r.account_id,
            account_user_id: r.account_user_id,
            room_id: r.room_id,
            name: r.name,
            topic: r.topic,
            avatar_url: r.avatar_url,
            canonical_alias: r.canonical_alias,
            room_type: r.room_type,
            last_activity_ts: r.last_activity_ts,
            last_event_id: r.last_event_id,
            notification_count: r.notification_count,
            highlight_count: r.highlight_count,
        }
    }
}

/// A single timeline event — used both as a timeline element and as the
/// single-event payload. `content`/`body` are `null` for UTDs and for redacted
/// events; `redacted` is the convenience flag derived from `redaction_event_id`.
#[derive(Debug, Serialize, ToSchema)]
pub struct EventDto {
    /// Axon account this event belongs to.
    pub account_id: Uuid,
    /// Matrix event ID.
    pub event_id: String,
    /// Matrix room ID.
    pub room_id: String,
    /// Matrix user ID of the sender.
    pub sender: String,
    /// Matrix state key for state events. `null` for message-like events.
    pub state_key: Option<String>,
    /// The `unsigned.prev_content` of a state event — the state content this
    /// event replaced, e.g. the previous `m.room.member` membership/displayname
    /// (issue #31). A client needs this to tell a real join (`membership`
    /// transitions to `"join"` from something else) apart from a displayname or
    /// avatar change (`membership` stays `"join"`), since both arrive as
    /// `m.room.member` events with `content.membership: "join"`. `null` for
    /// message-like events and for state events with no prior state.
    #[schema(value_type = Option<Object>)]
    pub prev_content: Option<Value>,
    /// `origin_server_ts` in milliseconds.
    pub origin_ts: i64,
    /// Matrix event type, e.g. `m.room.message`.
    #[serde(rename = "type")]
    pub r#type: String,
    /// Decrypted `content` JSON. `null` for UTDs and redacted events.
    #[schema(value_type = Option<Object>)]
    pub content: Option<Value>,
    /// Plaintext body. `null` when absent or masked by redaction.
    pub body: Option<String>,
    /// The event's `m.relates_to` object, if any.
    #[schema(value_type = Option<Object>)]
    pub relates_to: Option<Value>,
    /// `true` when this event has been redacted (content/body masked).
    pub redacted: bool,
    /// The `event_id` of the redaction that masked this event, if redacted.
    pub redaction_event_id: Option<String>,
    /// Sender-device trust snapshot at decrypt time (M7c): `verified`,
    /// `unverified`, `unknown`, or `verification_violation`. `null` for
    /// unencrypted events and rows with no recorded verdict (e.g. a UTD not yet
    /// re-decrypted). This is the at-receipt snapshot; the sender's *current*
    /// trust is available from the per-event verification bundle.
    pub sender_trust: Option<String>,
    /// Relation aggregation (M8): `true` when at least one valid `m.replace` edit
    /// targets this event, in which case `content`/`body` above are already the
    /// latest edited values (the standalone edit events are stripped from the
    /// timeline). Always `false` on the `/v1/ws` live stream, whose frames are raw
    /// pre-aggregation events — the read API is the authoritative resolved view.
    pub edited: bool,
    /// Number of valid edits targeting this event (M8); `0` when unedited.
    pub edit_count: i64,
    /// `origin_server_ts` of the winning (latest) edit in milliseconds (M8), or
    /// `null` when unedited or redacted.
    pub latest_edit_ts: Option<i64>,
    /// Per-emoji reaction tally (M8), keyed by reaction key:
    /// `{ "👍": { "count": 2, "senders": [...], "me": true, "my_event_ids": [...] } }`,
    /// resolved over this event's reactions regardless of pagination (the store
    /// hard-caps a pathological event at the oldest 1000 distinct `(sender, key)`
    /// pairs). `null` when the event has no reactions and on the raw `/v1/ws` live
    /// stream.
    /// Typed so generated clients can model the tally shape (each value is a
    /// [`ReactionDto`]) rather than an opaque object.
    pub reactions: Option<BTreeMap<String, ReactionDto>>,
}

impl From<axon_core::LiveEvent> for EventDto {
    /// Map a live-bus [`LiveEvent`](axon_core::LiveEvent) into the wire DTO — the
    /// `/v1/ws` payload shape matches the read API's. A freshly synced event is
    /// never already-redacted (a redaction arrives as its own later event), so
    /// the redaction fields are always unset here.
    fn from(e: axon_core::LiveEvent) -> Self {
        EventDto {
            account_id: e.account_id,
            event_id: e.event_id,
            room_id: e.room_id,
            sender: e.sender,
            state_key: e.state_key,
            prev_content: e.prev_content,
            origin_ts: e.origin_ts,
            r#type: e.event_type,
            content: e.content,
            body: e.body,
            relates_to: e.relates_to,
            redacted: false,
            redaction_event_id: None,
            sender_trust: e.sender_trust,
            // A live frame is a raw, pre-aggregation event: edits/reactions arrive
            // as their own later events and are resolved by the read API, not here.
            edited: false,
            edit_count: 0,
            latest_edit_ts: None,
            reactions: None,
        }
    }
}

impl EventDto {
    /// Map a store [`TimelineRow`] into the wire DTO. `account_id` is threaded in
    /// from the request path because the store row doesn't carry it.
    pub fn from_row(account_id: Uuid, row: TimelineRow) -> Self {
        EventDto {
            account_id,
            event_id: row.event_id,
            room_id: row.room_id,
            sender: row.sender,
            state_key: row.state_key,
            prev_content: row.prev_content,
            origin_ts: row.origin_ts,
            r#type: row.event_type,
            content: row.content,
            body: row.decrypted_body_text,
            relates_to: row.relates_to,
            redacted: row.redaction_event_id.is_some(),
            redaction_event_id: row.redaction_event_id,
            sender_trust: row.sender_trust,
            edited: row.edited,
            edit_count: row.edit_count,
            latest_edit_ts: row.latest_edit_ts,
            // The store builds this as a `{key: {count, senders, me, my_event_ids}}`
            // JSON object; deserialize it into the typed map. A malformed/absent
            // tally degrades to `None` rather than failing the whole row.
            reactions: row.reactions.and_then(|v| serde_json::from_value(v).ok()),
        }
    }
}

/// The per-event verification bundle (M7c): the durable at-decrypt trust
/// snapshot plus live cross-signing evidence. The two are deliberately separate
/// (ADR 0031) — the snapshot is what Matrix's evidence said when the event
/// arrived; `current` is read live and can differ.
#[derive(Debug, Serialize, ToSchema)]
pub struct VerificationBundleDto {
    /// Matrix event the bundle is about.
    pub event_id: String,
    /// Matrix user id of the event's sender.
    pub sender: String,
    /// The at-decrypt snapshot, or `null` for unencrypted events and a UTD not
    /// yet re-decrypted.
    pub snapshot: Option<TrustSnapshotDto>,
    /// The current (live) trust evidence.
    pub current: CurrentTrustDto,
}

/// The at-decrypt snapshot half of a [`VerificationBundleDto`].
#[derive(Debug, Serialize, ToSchema)]
pub struct TrustSnapshotDto {
    /// The four-valued sender-trust verdict at decrypt time.
    pub sender_trust: Option<String>,
    /// The coarse `verified`/`unverified` verification state at decrypt time.
    pub verification_state: Option<String>,
    /// The sending device's id at decrypt time.
    pub device_id: Option<String>,
    /// The sending device's curve25519 identity key.
    pub curve25519_key: Option<String>,
    /// The sending device's claimed ed25519 signing key.
    pub ed25519_key: Option<String>,
    /// The Megolm session id the event was encrypted with.
    pub session_id: Option<String>,
    /// Whether the Megolm key reached axon forwarded (key-share) rather than
    /// directly from the sender's device.
    pub forwarded: Option<bool>,
    /// If forwarded, the user id that forwarded the key.
    pub forwarder_user_id: Option<String>,
    /// If forwarded, the device id that forwarded the key.
    pub forwarder_device_id: Option<String>,
}

/// The live evidence half of a [`VerificationBundleDto`].
#[derive(Debug, Serialize, ToSchema)]
pub struct CurrentTrustDto {
    /// Whether the sending device is currently known to axon.
    pub device_known: bool,
    /// Whether the sending device is currently cross-signed by the sender's own
    /// master key. `null` when the device isn't known.
    pub device_cross_signed: Option<bool>,
    /// Whether the sender's user identity is currently known.
    pub identity_known: bool,
    /// Whether the sender's identity is currently verified. `null` when unknown.
    pub identity_verified: Option<bool>,
    /// Whether the sender's identity is currently in a verification violation
    /// (previously verified, identity since changed). `null` when unknown.
    pub verification_violation: Option<bool>,
    /// Whether the sender's identity was ever previously verified. `null` when
    /// unknown.
    pub previously_verified: Option<bool>,
    /// The sender's current master cross-signing key (base64), if known.
    pub master_key: Option<String>,
}

impl VerificationBundleDto {
    /// Map a port-layer [`TrustBundle`](crate::trust::TrustBundle) into the wire
    /// DTO, threading in the `event_id` from the request path.
    pub fn from_bundle(event_id: String, bundle: crate::trust::TrustBundle) -> Self {
        VerificationBundleDto {
            event_id,
            sender: bundle.sender,
            snapshot: bundle.snapshot.map(|s| TrustSnapshotDto {
                sender_trust: s.sender_trust,
                verification_state: s.verification_state,
                device_id: s.device_id,
                curve25519_key: s.curve25519_key,
                ed25519_key: s.ed25519_key,
                session_id: s.session_id,
                forwarded: s.forwarded,
                forwarder_user_id: s.forwarder_user_id,
                forwarder_device_id: s.forwarder_device_id,
            }),
            current: CurrentTrustDto {
                device_known: bundle.current.device_known,
                device_cross_signed: bundle.current.device_cross_signed,
                identity_known: bundle.current.identity_known,
                identity_verified: bundle.current.identity_verified,
                verification_violation: bundle.current.verification_violation,
                previously_verified: bundle.current.previously_verified,
                master_key: bundle.current.master_key,
            },
        }
    }
}

/// One Matrix device of a user, as reported by `GET …/devices` — the picker a
/// client shows before starting SAS verification (M16, ADR 0060).
#[derive(Debug, Serialize, ToSchema)]
pub struct DeviceDto {
    pub device_id: String,
    pub display_name: Option<String>,
    /// Locally *or* cross-signing trusted — the SDK's combined predicate. A
    /// UI can use this alone as "trusted / not trusted".
    pub is_verified: bool,
    /// Cross-signed specifically by the device owner's own master key — the
    /// finer-grained signal the verification bundle (ADR 0031) also exposes
    /// for a sender's device.
    pub is_cross_signed_by_owner: bool,
    /// Raw local trust state (`"verified"` | `"black_listed"` | `"ignored"` |
    /// `"unset"`).
    pub local_trust_state: String,
    /// Encryption algorithms this device advertises, e.g.
    /// `"m.megolm.v1.aes-sha2"`.
    pub algorithms: Vec<String>,
}

/// Response body for `GET …/devices`: the resolved target user plus their
/// devices, read live from the SDK — never persisted (M16, ADR 0060).
#[derive(Debug, Serialize, ToSchema)]
pub struct DeviceListDto {
    /// The Matrix user id the devices belong to — either the account's own
    /// `user_id` (self, when `?user_id=` was omitted) or the requested one.
    pub user_id: String,
    pub devices: Vec<DeviceDto>,
}

impl From<crate::devices::DeviceList> for DeviceListDto {
    fn from(list: crate::devices::DeviceList) -> Self {
        DeviceListDto {
            user_id: list.user_id,
            devices: list
                .devices
                .into_iter()
                .map(|d| DeviceDto {
                    device_id: d.device_id,
                    display_name: d.display_name,
                    is_verified: d.is_verified,
                    is_cross_signed_by_owner: d.is_cross_signed_by_owner,
                    local_trust_state: d.local_trust_state,
                    algorithms: d.algorithms,
                })
                .collect(),
        }
    }
}

/// One member of a room as returned by `GET …/rooms/{room_id}/members`. Derived
/// from the current resolved `m.room.member` state row for the user.
#[derive(Debug, Serialize, ToSchema)]
pub struct MemberDto {
    /// Matrix user ID (the `m.room.member` state key).
    pub user_id: String,
    /// Membership value from the event content: `join`, `invite`, `leave`, `ban`.
    pub membership: String,
    /// Display name from the event content, if set.
    pub display_name: Option<String>,
    /// Resolved avatar `mxc://` URI for this member, if known. Prefers the
    /// current room membership event's `avatar_url`; may fall back to cached
    /// room-member profile data from the sync engine.
    pub avatar_url: Option<String>,
}

impl MemberDto {
    pub fn from_state_row(row: axon_store::RoomStateRow) -> Self {
        let membership = row
            .content
            .as_ref()
            .and_then(|c| c.get("membership"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let display_name = row
            .content
            .as_ref()
            .and_then(|c| c.get("displayname"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let avatar_url = row
            .content
            .as_ref()
            .and_then(|c| c.get("avatar_url"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        MemberDto {
            user_id: row.state_key,
            membership,
            display_name,
            avatar_url,
        }
    }
}

/// One space-child entry (`GET …/rooms/{room_id}/space/children`, issue #404,
/// ADR 0084): an `m.space.child` tuple enriched with the child room's own
/// cached display fields — `name`/`avatar_url`/`room_type` are `None` when
/// Axon doesn't know that room (e.g. the account was never joined to it).
#[derive(Debug, Serialize, ToSchema)]
pub struct SpaceChildDto {
    /// The child room's id (the `m.space.child` state key).
    pub room_id: String,
    /// Federation-resolution hints (`content.via`) for a child Axon has no
    /// direct path to.
    pub via: Vec<String>,
    /// MSC1772 sort key, if set. Children are returned already ordered by
    /// this field (ascending, absent sorts last), then `origin_ts`, then
    /// `room_id`.
    pub order: Option<String>,
    /// Whether the child is flagged as suggested (`content.suggested`,
    /// `false` when absent).
    pub suggested: bool,
    /// The child room's cached `m.room.name`, if known.
    pub name: Option<String>,
    /// The child room's cached avatar `mxc://` URI (`m.room.avatar`), if known.
    pub avatar_url: Option<String>,
    /// The child room's `m.room.create` `type` (e.g. `m.space`), if known.
    pub room_type: Option<String>,
}

impl From<SpaceChildRow> for SpaceChildDto {
    fn from(r: SpaceChildRow) -> Self {
        SpaceChildDto {
            room_id: r.room_id,
            via: r.via,
            order: r.order,
            suggested: r.suggested,
            name: r.name,
            avatar_url: r.avatar_url,
            room_type: r.room_type,
        }
    }
}

/// One space-parent entry (`GET …/rooms/{room_id}/space/parents`, issue #404,
/// ADR 0084) — the reverse lookup of [`SpaceChildDto`]: the spaces `room_id`
/// claims to belong to, each enriched the same way.
#[derive(Debug, Serialize, ToSchema)]
pub struct SpaceParentDto {
    /// The parent space's room id (the `m.space.parent` state key).
    pub room_id: String,
    /// Federation-resolution hints (`content.via`).
    pub via: Vec<String>,
    /// Whether this parent is the canonical one (`content.canonical`, `false`
    /// when absent).
    pub canonical: bool,
    /// The parent's cached `m.room.name`, if known.
    pub name: Option<String>,
    /// The parent's cached avatar `mxc://` URI, if known.
    pub avatar_url: Option<String>,
    /// The parent's `m.room.create` `type` (normally `m.space`), if known.
    pub room_type: Option<String>,
}

impl From<SpaceParentRow> for SpaceParentDto {
    fn from(r: SpaceParentRow) -> Self {
        SpaceParentDto {
            room_id: r.room_id,
            via: r.via,
            canonical: r.canonical,
            name: r.name,
            avatar_url: r.avatar_url,
            room_type: r.room_type,
        }
    }
}

/// Room info (`GET …/rooms/{room_id}/info`, issue #404, ADR 0084): four small
/// "what kind of room is this" singleton state reads bundled into one call.
/// Each field is `None` when the room has no such state set (or is unknown
/// to Axon) — an unknown room reads as all-`None`, not a 404.
#[derive(Debug, Serialize, ToSchema)]
pub struct RoomInfoDto {
    /// `m.room.join_rules` `content.join_rule`
    /// (`invite`/`public`/`knock`/`restricted`/`knock_restricted`/…).
    pub join_rule: Option<String>,
    /// `m.room.history_visibility` `content.history_visibility`.
    pub history_visibility: Option<String>,
    /// `m.room.guest_access` `content.guest_access`.
    pub guest_access: Option<String>,
    /// `m.room.encryption` `content.algorithm`, or `None` if the room is
    /// unencrypted. Matrix has no mechanism to turn encryption back off, so
    /// once this is `Some` it stays `Some`.
    pub encryption_algorithm: Option<String>,
}

/// Upgrade chain (`GET …/rooms/{room_id}/upgrade`, issue #404, ADR 0084):
/// where a tombstoned room's replacement lives, and/or where this room was
/// upgraded from. Deliberately not folded into `RoomDto` — `list_rooms`
/// already excludes tombstoned rooms from `/v1/rooms`, so a `RoomDto` field
/// there would be unreachable for the one room it matters for; this is a
/// direct by-id read instead, for a client that already holds the old room's
/// id (e.g. from local history).
#[derive(Debug, Serialize, ToSchema)]
pub struct RoomUpgradeDto {
    /// The successor room id, from `m.room.tombstone` `content.replacement_room`.
    /// `None` unless this room has been tombstoned.
    pub tombstoned_to: Option<String>,
    /// The predecessor room id, from `m.room.create` `content.predecessor.room_id`.
    /// `None` unless this room was created as an upgrade of another.
    pub upgraded_from: Option<String>,
}

/// Request body for sending a message (`POST …/rooms/{room_id}/send`). Sent as an
/// `m.room.message`; `account_id`/`room_id` come from the path.
///
/// `body` is the plain-text content (and the fallback for clients that don't
/// render formatting). To send rich text, supply `format` *and* `formatted_body`
/// together: `format` must be `org.matrix.custom.html` and `formatted_body` the
/// rendered HTML. Axon carries them verbatim — it never interprets `body` as
/// Markdown.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SendMessageRequest {
    /// The plain-text message body (and formatting fallback).
    pub body: String,
    /// Markup name for `formatted_body` — only `org.matrix.custom.html`. Must be
    /// paired with `formatted_body`.
    #[serde(default)]
    pub format: Option<String>,
    /// The rendered HTML body. Must be paired with `format`.
    #[serde(default)]
    pub formatted_body: Option<String>,
    /// Send as a reply to this event id (`m.in_reply_to`). Optional.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Send into this thread, identified by its root event id
    /// (`rel_type: m.thread`). Optional; when set with `reply_to`, the reply is
    /// scoped to the thread. Without `reply_to`, this is a thread member, not a
    /// reply.
    #[serde(default)]
    pub thread_root: Option<String>,
}

/// Request body for sending a staged media upload into a room
/// (`POST …/rooms/{room_id}/send-media`). The `upload_id` must refer to an
/// unsent staged upload owned by the path account.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SendMediaRequest {
    /// Server-issued staged upload id returned by `POST …/media/uploads`.
    pub upload_id: Uuid,
    /// Optional media caption. When absent, Axon uses the staged filename as the
    /// Matrix event body.
    #[serde(default)]
    pub caption: Option<String>,
    /// Send as a reply to this event id (`m.in_reply_to`). Optional.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Send into this thread, identified by its root event id
    /// (`rel_type: m.thread`). Optional; when set with `reply_to`, the reply
    /// is scoped to the thread. Without `reply_to`, this is a thread member,
    /// not a reply.
    #[serde(default)]
    pub thread_root: Option<String>,
}

/// Request body for editing a message (`PUT …/events/{event_id}`). Replaces the
/// target event's text via an `m.replace` relation.
///
/// Like [`SendMessageRequest`], `format` + `formatted_body` are optional and must
/// be supplied together to set rich text on the replacement.
#[derive(Debug, Deserialize, ToSchema)]
pub struct EditRequest {
    /// The new plain-text message body (and formatting fallback).
    pub body: String,
    /// Markup name for `formatted_body` — only `org.matrix.custom.html`. Must be
    /// paired with `formatted_body`.
    #[serde(default)]
    pub format: Option<String>,
    /// The rendered HTML body. Must be paired with `format`.
    #[serde(default)]
    pub formatted_body: Option<String>,
}

/// Request body for reacting to an event (`POST …/events/{event_id}/reactions`).
#[derive(Debug, Deserialize, ToSchema)]
pub struct ReactRequest {
    /// The reaction key — typically an emoji.
    pub key: String,
}

/// Query parameters for redaction (`DELETE …/events/{event_id}`).
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RedactQuery {
    /// Optional human-readable reason recorded on the redaction.
    pub reason: Option<String>,
}

/// Result of a successful mutation: the id of the event the homeserver created
/// (the message, the replacement, the redaction, or the reaction).
#[derive(Debug, Serialize, ToSchema)]
pub struct SendResultDto {
    /// The created Matrix event id.
    pub event_id: String,
}

/// Request body for marking a room read (`POST …/rooms/{room_id}/read`; ADR
/// 0067). Sets both the public read receipt and the private fully-read marker
/// to `event_id` in one homeserver call.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ReadReceiptRequest {
    /// The event id to mark as read.
    pub event_id: String,
}

/// Request body for setting this account's typing indicator
/// (`PUT …/rooms/{room_id}/typing`; ADR 0068 M19a).
#[derive(Debug, Deserialize, ToSchema)]
pub struct TypingRequest {
    /// Whether the account is now typing in this room. Setting `false` clears
    /// an active typing indicator early instead of waiting for it to expire.
    pub typing: bool,
}

/// Request body for inviting a user to a room
/// (`POST …/rooms/{room_id}/invite`; ADR 0068 M19b).
#[derive(Debug, Deserialize, ToSchema)]
pub struct InviteRequest {
    /// The invited user's Matrix id (`@user:server`).
    pub user_id: String,
}

/// Request body shared by the user-targeted membership actions that carry an
/// optional reason — `kick`, `ban`, `unban` (ADR 0068 M19b). Matrix models all
/// three with the same `{user_id, reason?}` shape.
#[derive(Debug, Deserialize, ToSchema)]
pub struct MemberActionRequest {
    /// The target user's Matrix id (`@user:server`).
    pub user_id: String,
    /// Optional human-readable reason recorded on the membership change.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Request body for joining a room by id or alias (`POST …/rooms/join`;
/// ADR 0068 M19c).
#[derive(Debug, Deserialize, ToSchema)]
pub struct JoinRoomRequest {
    /// The room id (`!room:server`) or alias (`#room:server`) to join.
    pub room_id_or_alias: String,
    /// Federation-resolution hints (ruma's `via`) for an alias/id this
    /// account's client has no direct path to.
    #[serde(default)]
    pub server_names: Vec<String>,
}

/// Request body for knocking on a room (`POST …/rooms/knock`; ADR 0068 M19c).
#[derive(Debug, Deserialize, ToSchema)]
pub struct KnockRoomRequest {
    /// The room id (`!room:server`) or alias (`#room:server`) to knock on.
    pub room_id_or_alias: String,
    /// Optional human-readable reason shown to the room's members.
    #[serde(default)]
    pub reason: Option<String>,
    /// Federation-resolution hints (ruma's `via`) for an alias/id this
    /// account's client has no direct path to.
    #[serde(default)]
    pub server_names: Vec<String>,
}

/// Request body for creating a DM (`POST …/rooms/dm`; ADR 0068 M19c).
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateDmRequest {
    /// The other participant's Matrix id (`@user:server`).
    pub user_id: String,
}

/// A room-creation preset (see `axon_core::RoomPreset`), mirroring the three
/// Matrix defines. Variant names deliberately drop the shared `Chat` suffix
/// the Matrix spec's own vocabulary uses; the wire values (`private_chat`,
/// `public_chat`, `trusted_private_chat`) are preserved via explicit renames.
#[derive(Debug, Deserialize, ToSchema)]
pub enum RoomPresetDto {
    #[serde(rename = "private_chat")]
    Private,
    #[serde(rename = "public_chat")]
    Public,
    #[serde(rename = "trusted_private_chat")]
    TrustedPrivate,
}

impl From<RoomPresetDto> for RoomPreset {
    fn from(preset: RoomPresetDto) -> Self {
        match preset {
            RoomPresetDto::Private => RoomPreset::Private,
            RoomPresetDto::Public => RoomPreset::Public,
            RoomPresetDto::TrustedPrivate => RoomPreset::TrustedPrivate,
        }
    }
}

/// Request body for creating a room (`POST …/rooms`; ADR 0068 M19c) — the
/// minimal-but-useful subset of the Matrix `createRoom` endpoint exposed to
/// API clients. An empty body is valid: it creates a private, unencrypted,
/// unnamed room.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateRoomRequestDto {
    /// Room name (`m.room.name`), if set.
    #[serde(default)]
    pub name: Option<String>,
    /// Room topic (`m.room.topic`), if set.
    #[serde(default)]
    pub topic: Option<String>,
    /// Users to invite on creation (`@user:server`).
    #[serde(default)]
    pub invite: Vec<String>,
    /// Whether to set the `is_direct` flag on the invites this creates.
    #[serde(default)]
    pub is_direct: bool,
    /// Whether the room is published in the room directory. Defaults to
    /// `false` (private).
    #[serde(default)]
    pub public: bool,
    /// Convenience default-state-events preset.
    #[serde(default)]
    pub preset: Option<RoomPresetDto>,
    /// When true, an `m.room.encryption` event is included in the room's
    /// initial state, so it is encrypted from its first transaction rather
    /// than via a later, racier `enable_encryption` call.
    #[serde(default)]
    pub encrypted: bool,
}

impl From<CreateRoomRequestDto> for CreateRoomRequest {
    fn from(dto: CreateRoomRequestDto) -> Self {
        CreateRoomRequest {
            name: dto.name,
            topic: dto.topic,
            invite: dto.invite,
            is_direct: dto.is_direct,
            public: dto.public,
            preset: dto.preset.map(Into::into),
            encrypted: dto.encrypted,
        }
    }
}

/// Response for the four room-entry mutations (`join`/`knock`/`create_room`/
/// `create_dm`; ADR 0068 M19c): the resulting room's id.
#[derive(Debug, Serialize, ToSchema)]
pub struct RoomEntryResultDto {
    pub room_id: String,
}

/// Request body for setting a room's name (`PUT …/rooms/{room_id}/name`;
/// ADR 0068 M19d). An empty `name` clears it — the SDK has no separate
/// "remove" primitive for name/topic (unlike avatar).
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetRoomNameRequest {
    pub name: String,
}

/// Request body for setting a room's topic (`PUT …/rooms/{room_id}/topic`;
/// ADR 0068 M19d). An empty `topic` clears it, same convention as
/// [`SetRoomNameRequest`].
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetRoomTopicRequest {
    pub topic: String,
}

/// Request body for setting a room's avatar (`PUT …/rooms/{room_id}/avatar`;
/// ADR 0068 M19d). Takes an already-staged upload id, mirroring
/// [`SendMediaRequest`]'s `upload_id` — Axon has no route that hands a
/// client an `mxc://` URI to reference directly, so avatar-set reuses the
/// same staged-upload flow as sending media.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetRoomAvatarRequest {
    /// Server-issued staged upload id returned by `POST …/media/uploads`.
    pub upload_id: Uuid,
}

/// Request body for adding or updating a room tag
/// (`PUT …/rooms/{room_id}/tags/{tag}`; ADR 0068 M19d). `tag` itself is a
/// path parameter, not part of this body — see the route handler.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetRoomTagRequest {
    /// Optional sort order among this account's tagged rooms, in `[0, 1]`
    /// per the Matrix spec's `m.tag` convention. Rejected as `400` outside
    /// that range.
    #[serde(default)]
    pub order: Option<f64>,
}

/// Request body for setting a room's power levels (`PUT
/// …/rooms/{room_id}/power_levels`; ADR 0068 M19e): role thresholds and
/// per-user levels, merged into one `m.room.power_levels` state event. A
/// field left absent leaves that level unchanged; `users` entries are
/// merged into the room's existing per-user map, not a wholesale
/// replacement of it.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct PowerLevelChangesRequest {
    #[serde(default)]
    pub ban: Option<i64>,
    #[serde(default)]
    pub invite: Option<i64>,
    #[serde(default)]
    pub kick: Option<i64>,
    #[serde(default)]
    pub redact: Option<i64>,
    #[serde(default)]
    pub events_default: Option<i64>,
    #[serde(default)]
    pub state_default: Option<i64>,
    #[serde(default)]
    pub users_default: Option<i64>,
    /// User id -> requested power level, merged into the room's existing
    /// `users` map. A user not present here keeps their current level.
    #[serde(default)]
    pub users: HashMap<String, i64>,
    /// Bypasses the self-demotion guardrail: without this, a change that
    /// would drop the caller's own resolved power level below what's
    /// needed to send another `m.room.power_levels` event is rejected as
    /// `400`, since that write would otherwise succeed and permanently
    /// strand the caller with no way to self-correct.
    #[serde(default)]
    pub acknowledge_self_demotion: bool,
}

impl From<PowerLevelChangesRequest> for PowerLevelChanges {
    fn from(dto: PowerLevelChangesRequest) -> Self {
        PowerLevelChanges {
            ban: dto.ban,
            invite: dto.invite,
            kick: dto.kick,
            redact: dto.redact,
            events_default: dto.events_default,
            state_default: dto.state_default,
            users_default: dto.users_default,
            users: dto.users,
            acknowledge_self_demotion: dto.acknowledge_self_demotion,
        }
    }
}

/// Response for `GET …/rooms/{room_id}/power_levels` (ADR 0068 M19e): the
/// room's fully resolved power levels, defaults filled in — the same
/// computation the write path uses internally for its self-demotion
/// guardrail.
#[derive(Debug, Serialize, ToSchema)]
pub struct PowerLevelsDto {
    pub ban: i64,
    pub invite: i64,
    pub kick: i64,
    pub redact: i64,
    pub events_default: i64,
    pub state_default: i64,
    pub users_default: i64,
    pub users: HashMap<String, i64>,
}

impl From<ResolvedPowerLevels> for PowerLevelsDto {
    fn from(resolved: ResolvedPowerLevels) -> Self {
        PowerLevelsDto {
            ban: resolved.ban,
            invite: resolved.invite,
            kick: resolved.kick,
            redact: resolved.redact,
            events_default: resolved.events_default,
            state_default: resolved.state_default,
            users_default: resolved.users_default,
            users: resolved.users,
        }
    }
}

/// Request body for setting this account's display name
/// (`PUT …/profile/display_name`; ADR 0068 M19f). An empty `display_name`
/// clears it, same convention as [`SetRoomNameRequest`] — the handler issues
/// the SDK's real profile-field-delete call rather than writing an empty
/// string to the homeserver.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetDisplayNameRequest {
    pub display_name: String,
}

/// Request body for setting this account's avatar (`PUT …/profile/avatar`;
/// ADR 0068 M19f). Takes an already-staged upload id, mirroring
/// [`SetRoomAvatarRequest`] — Axon has no route that hands a client an
/// `mxc://` URI to reference directly.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetAccountAvatarRequest {
    /// Server-issued staged upload id returned by `POST …/media/uploads`.
    pub upload_id: Uuid,
}

/// Response for `GET …/users/{user_id}/profile` (ADR 0068 M19f): the target
/// user's display name and avatar, either of which may be absent.
#[derive(Debug, Serialize, ToSchema)]
pub struct MatrixProfileDto {
    pub user_id: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
}

/// Wraps a [`MatrixProfile`] with the `user_id` it was fetched for — the
/// gateway's read only knows the profile fields, not which user id the
/// caller asked about, so the route attaches it.
impl MatrixProfileDto {
    pub(crate) fn from_profile(user_id: String, profile: MatrixProfile) -> Self {
        MatrixProfileDto {
            user_id,
            display_name: profile.display_name,
            avatar_url: profile.avatar_url,
        }
    }
}

/// Query parameters for searching a homeserver's public-room directory
/// (`GET …/directory/public_rooms`; ADR 0068 M19f). A paginated **read**, not
/// a mutation like the other M19f verbs — see `PublicRoomsSender::public_rooms`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PublicRoomsQueryDto {
    /// Search the directory of this server instead of the account's own
    /// homeserver.
    #[serde(default)]
    pub server: Option<String>,
    /// Free-text filter over room name, topic, and canonical alias.
    #[serde(default)]
    pub search_term: Option<String>,
    /// Maximum rooms to return in this page.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Pagination token from a previous page's `next_batch`.
    #[serde(default)]
    pub since: Option<String>,
}

impl From<PublicRoomsQueryDto> for axon_core::PublicRoomsQuery {
    fn from(dto: PublicRoomsQueryDto) -> Self {
        axon_core::PublicRoomsQuery {
            server: dto.server,
            search_term: dto.search_term,
            limit: dto.limit,
            since: dto.since,
        }
    }
}

/// One room in a [`PublicRoomsPageDto`].
#[derive(Debug, Serialize, ToSchema)]
pub struct PublicRoomSummaryDto {
    pub room_id: String,
    pub canonical_alias: Option<String>,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub avatar_url: Option<String>,
    pub num_joined_members: u64,
    pub world_readable: bool,
    pub guest_can_join: bool,
    pub join_rule: String,
    pub room_type: Option<String>,
}

impl From<PublicRoomSummary> for PublicRoomSummaryDto {
    fn from(summary: PublicRoomSummary) -> Self {
        PublicRoomSummaryDto {
            room_id: summary.room_id,
            canonical_alias: summary.canonical_alias,
            name: summary.name,
            topic: summary.topic,
            avatar_url: summary.avatar_url,
            num_joined_members: summary.num_joined_members,
            world_readable: summary.world_readable,
            guest_can_join: summary.guest_can_join,
            join_rule: summary.join_rule,
            room_type: summary.room_type,
        }
    }
}

/// Response for `GET …/directory/public_rooms` (ADR 0068 M19f): one page of
/// public rooms plus pagination tokens.
#[derive(Debug, Serialize, ToSchema)]
pub struct PublicRoomsPageDto {
    pub chunk: Vec<PublicRoomSummaryDto>,
    pub next_batch: Option<String>,
    pub prev_batch: Option<String>,
    pub total_room_count_estimate: Option<u64>,
}

impl From<PublicRoomsPage> for PublicRoomsPageDto {
    fn from(page: PublicRoomsPage) -> Self {
        PublicRoomsPageDto {
            chunk: page.chunk.into_iter().map(Into::into).collect(),
            next_batch: page.next_batch,
            prev_batch: page.prev_batch,
            total_room_count_estimate: page.total_room_count_estimate,
        }
    }
}

/// Query parameters for staging a media upload (`POST …/media/uploads`).
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StageMediaUploadQuery {
    /// Matrix media message kind the staged bytes are intended to become.
    pub kind: MediaUploadKindDto,
    /// Original filename. The handler normalizes path-like input down to the
    /// basename before storing it.
    pub filename: String,
}

/// Supported outbound media kind for M15.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MediaUploadKindDto {
    Image,
    File,
}

impl MediaUploadKindDto {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaUploadKindDto::Image => "image",
            MediaUploadKindDto::File => "file",
        }
    }
}

/// Query parameters for the thumbnail proxy (`GET
/// …/media/{account_id}/{server_name}/{media_id}/thumbnail`).
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ThumbnailQuery {
    /// Desired width in pixels. Clamped to a fixed `[min, max]` range and
    /// snapped up to the nearest of a small set of standard sizes, rather
    /// than rejected — see `routes::media::snap_thumbnail_dimension`.
    pub width: u32,
    /// Desired height in pixels. Same clamp-and-snap as `width`.
    pub height: u32,
    /// Resizing method; defaults to `scale` (the Matrix spec default) when
    /// omitted.
    pub method: Option<ThumbnailMethodDto>,
}

/// Wire-shape mirror of [`axon_core::media::ThumbnailMethod`] — kept separate
/// so `axon-core` stays free of `utoipa`/serde-schema concerns (same split as
/// [`MediaUploadKindDto`]).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThumbnailMethodDto {
    Crop,
    Scale,
}

impl From<ThumbnailMethodDto> for axon_core::media::ThumbnailMethod {
    fn from(dto: ThumbnailMethodDto) -> Self {
        match dto {
            ThumbnailMethodDto::Crop => axon_core::media::ThumbnailMethod::Crop,
            ThumbnailMethodDto::Scale => axon_core::media::ThumbnailMethod::Scale,
        }
    }
}

/// Metadata returned after bytes have been staged successfully.
#[derive(Debug, Serialize, ToSchema)]
pub struct StagedUploadDto {
    /// Server-issued upload id used by the later send-media mutation.
    pub upload_id: Uuid,
    /// Intended Matrix media message kind.
    pub kind: MediaUploadKindDto,
    /// Normalized filename persisted with the upload.
    pub filename: String,
    /// Sanitized content type from the request, when supplied.
    pub content_type: Option<String>,
    /// Accepted byte length.
    pub size_bytes: u64,
    /// RFC 3339 expiry time for the staged upload.
    pub expires_at: String,
}

/// Lifecycle state on the wire — the constrained enum form of
/// [`axon_store::AccountState`], so the OpenAPI schema advertises the closed set
/// (`active` / `deactivated` / `deleting`) and generated clients can model it
/// exhaustively rather than seeing an open `string`. Serialize-only: unknown
/// *stored* values are still rejected at the store boundary
/// (`AccountState::from_db`), so widening the wire vocabulary can't smuggle one in.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AccountStateDto {
    Active,
    Deactivated,
    Deleting,
}

impl From<AccountState> for AccountStateDto {
    fn from(s: AccountState) -> Self {
        match s {
            AccountState::Active => AccountStateDto::Active,
            AccountState::Deactivated => AccountStateDto::Deactivated,
            AccountState::Deleting => AccountStateDto::Deleting,
        }
    }
}

/// Sync-engine readiness on the wire (ADR 0030, issue #241) — the constrained
/// enum form of the four values `axon-sync`'s `SyncHealth` derives, so the
/// OpenAPI schema advertises the closed set and generated clients can model it
/// exhaustively. See [`AccountDto::sync_state`] for what each value means.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SyncStateDto {
    Connecting,
    Syncing,
    Ready,
    Offline,
}

impl From<&'static str> for SyncStateDto {
    fn from(s: &'static str) -> Self {
        match s {
            "syncing" => SyncStateDto::Syncing,
            "ready" => SyncStateDto::Ready,
            "offline" => SyncStateDto::Offline,
            // "connecting", and defensively any value this crate's `SyncStateProvider`
            // port wasn't supposed to produce — connecting is the only state a
            // client should never block on.
            _ => SyncStateDto::Connecting,
        }
    }
}

/// An account in the lifecycle read API (`GET /v1/accounts`,
/// `GET /v1/accounts/{account_id}`). The encrypted access token is deliberately
/// absent — only public lifecycle facts are exposed, never a secret.
#[derive(Debug, Serialize, ToSchema)]
pub struct AccountDto {
    /// Stable Axon account id.
    pub account_id: Uuid,
    /// Full Matrix user ID, e.g. `@alice:example.org`.
    pub user_id: String,
    /// Homeserver base URL.
    pub homeserver_url: String,
    /// Device ID of axon's current session, once it has logged in.
    pub device_id: Option<String>,
    /// Lifecycle state. The list endpoint returns only `active` accounts; a by-id
    /// read can return any state.
    pub state: AccountStateDto,
    /// Whether axon's own device is currently cross-signed (orthogonal to
    /// `state`), derived from the SDK and kept fresh by the verification watcher
    /// (ADR 0026): `false` for a fresh/unverified device, `true` once
    /// `recover`/`verify` has cross-signed it. Kept nullable on the wire for
    /// forward-compatibility, but currently always present.
    pub verified: Option<bool>,
    /// Sync-engine readiness (ADR 0030, issue #241): whether mutations to this
    /// account are currently reliable. `"connecting"` before the SDK client/
    /// session is up (including for an account with no supervised sync task,
    /// e.g. `deactivated`); `"syncing"` once the sync service is running but
    /// before its first cycle completes, when a send may block for a while;
    /// `"ready"` once that first cycle has completed; `"offline"` while the
    /// sync service is retrying a lost homeserver connection. Also pushed live
    /// on transition as an `account.sync_state` `/v1/ws` frame.
    pub sync_state: SyncStateDto,
    /// Row creation time, RFC 3339.
    pub created_at: String,
    /// Last update time, RFC 3339.
    pub updated_at: String,
}

impl AccountDto {
    /// Build the DTO from a stored row plus its live sync-engine readiness
    /// (ADR 0030). `sync_state` isn't a stored column — it's read from the
    /// `SyncStateProvider` port at request time — so every construction site
    /// supplies it explicitly rather than risk a stale default value.
    pub fn from_account(a: Account, sync_state: SyncStateDto) -> Self {
        AccountDto {
            account_id: a.account_id,
            user_id: a.user_id,
            homeserver_url: a.homeserver_url,
            device_id: a.device_id,
            state: a.state.into(),
            // Surface the derived cross-signing state (ADR 0026). Kept `Option`
            // for wire stability; populated from the persisted column.
            verified: Some(a.verified),
            sync_state,
            created_at: a.created_at.to_rfc3339(),
            updated_at: a.updated_at.to_rfc3339(),
        }
    }
}

/// Request body for runtime login (`POST /v1/accounts/login`). Adds or
/// reactivates a Matrix account keyed by its Matrix `username`. The
/// password is used once to authenticate and is never stored or echoed back.
#[derive(Debug, Deserialize, ToSchema)]
pub struct LoginRequest {
    /// Homeserver base URL, e.g. `https://matrix.example.org`. Optional: when
    /// omitted, the server discovers it from the user ID's server name via
    /// `.well-known/matrix/client` (falling back to `https://<server name>`).
    /// Supply it explicitly to skip discovery (e.g. `http://localhost:8008`
    /// for a local dev homeserver).
    pub homeserver_url: Option<String>,
    /// Full Matrix user ID, e.g. `@alice:example.org`. A user ID written with
    /// the homeserver's hostname as its domain (`@alice:matrix.example.org`)
    /// is rejected with a 400 whose message suggests the canonical spelling.
    pub username: String,
    /// Account password. Consumed once at login; never persisted.
    pub password: String,
}

/// Request body for importing an existing session (`POST
/// /v1/accounts/import`). Adopts a Matrix `access_token` axon didn't mint
/// itself — issued by another client, or belonging to an SSO-only account with
/// no password — as a runtime account keyed by Matrix `username`. Both secrets
/// are used once to establish and validate the session and are never stored or
/// echoed back; only the resulting encrypted token is persisted (as with a
/// fresh login).
#[derive(Debug, Deserialize, ToSchema)]
pub struct ImportTokenRequest {
    /// Homeserver base URL, e.g. `https://matrix.example.org`. Required: unlike
    /// [`LoginRequest`], there is no MXID-based discovery for a token.
    pub homeserver_url: String,
    /// Full Matrix user ID, e.g. `@alice:example.org`.
    pub username: String,
    /// The existing Matrix access token to adopt.
    pub access_token: String,
    /// The device ID the access token belongs to.
    pub device_id: String,
}

/// Request body for recovery-key key acquisition
/// (`POST /v1/accounts/{account_id}/recover`). The Secure-Storage (4S) recovery
/// key imports the account's megolm key backup + cross-signing keys, self-verifies
/// axon's device, and unlocks stored UTDs. Like the login password it is a
/// crown-jewel secret: used once to recover and **never persisted** or echoed back.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RecoverRequest {
    /// The account's Secure-Storage (4S) recovery key.
    pub recovery_key: String,
}

/// Result of an explicit UTD re-decryption retry.
#[derive(Debug, Serialize, ToSchema)]
pub struct RedecryptUtdsResponse {
    /// Pending UTD rows selected at the start of the retry.
    pub selected: usize,
    /// Selected rows that reached a re-decryption attempt.
    pub attempted: usize,
    /// Rows successfully back-filled with decrypted content.
    pub decrypted: usize,
    /// Selected rows that are still UTDs after the retry.
    pub still_pending: usize,
    /// Whether the server stopped waiting before the retry completed.
    pub timed_out: bool,
}

impl From<crate::lifecycle::RedecryptUtdsStats> for RedecryptUtdsResponse {
    fn from(stats: crate::lifecycle::RedecryptUtdsStats) -> Self {
        Self {
            selected: stats.selected,
            attempted: stats.attempted,
            decrypted: stats.decrypted,
            still_pending: stats.still_pending,
            timed_out: stats.timed_out,
        }
    }
}

/// Request body for starting a SAS verification
/// (`POST /v1/accounts/{account_id}/verify`). Names the verification target:
/// either a `device_id` — one of the user's own trusted devices
/// (self-verification) — or a `user_id` — another user's identity (cross-user
/// verification, ADR 0040). Exactly one must be provided.
#[derive(Debug, Deserialize, ToSchema)]
pub struct StartVerifyRequest {
    /// The user ID of another user to verify (cross-user verification).
    #[serde(default)]
    pub user_id: Option<String>,
    /// The device ID of the trusted device to verify against (self-verification).
    #[serde(default)]
    pub device_id: Option<String>,
}

/// Response body for starting a SAS verification: the new flow's transaction id,
/// which keys every subsequent read (`GET …/verify/{flow_id}`), op
/// (`…/confirm`, `…/cancel`), and `verification.*` WS frame.
#[derive(Debug, Serialize, ToSchema)]
pub struct StartVerifyResponse {
    /// The verification transaction id.
    pub flow_id: String,
}

/// The stage of a SAS verification flow on the wire.
#[derive(Debug, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlowStageDto {
    /// A verification was requested; awaiting the peer.
    Requested,
    /// The peer is ready; SAS not yet computed.
    Ready,
    /// Keys exchanged — the SAS is ready to compare.
    KeysExchanged,
    /// This side has confirmed; awaiting the peer's MAC.
    Confirmed,
    /// Completed successfully; the device is now cross-signed.
    Done,
    /// Cancelled (timeout, mismatch, or either side cancelling).
    Cancelled,
}

impl From<crate::verification::FlowStage> for FlowStageDto {
    fn from(stage: crate::verification::FlowStage) -> Self {
        use crate::verification::FlowStage;
        match stage {
            FlowStage::Requested => FlowStageDto::Requested,
            FlowStage::Ready => FlowStageDto::Ready,
            FlowStage::KeysExchanged => FlowStageDto::KeysExchanged,
            FlowStage::Confirmed => FlowStageDto::Confirmed,
            FlowStage::Done => FlowStageDto::Done,
            FlowStage::Cancelled => FlowStageDto::Cancelled,
        }
    }
}

/// One SAS emoji: the symbol and its short English description.
#[derive(Debug, Serialize, ToSchema)]
pub struct EmojiDto {
    /// The emoji character(s).
    pub symbol: String,
    /// The emoji's short English name.
    pub description: String,
}

/// A verification flow's replayable state (`GET …/verify` /
/// `GET …/verify/{flow_id}`). `emoji`/`decimals` are `null` until keys are
/// exchanged; `cancel_reason` is `null` unless the flow was cancelled.
#[derive(Debug, Serialize, ToSchema)]
pub struct FlowDto {
    /// The verification transaction id.
    pub flow_id: String,
    /// The user being verified — the account's own user id for self-verification,
    /// or the peer's user id for cross-user verification (ADR 0040).
    pub user_id: String,
    /// The other device in the flow for self-verification. `null` for cross-user
    /// verification, which targets a user identity rather than one known device.
    pub device_id: Option<String>,
    /// The flow's current stage.
    pub stage: FlowStageDto,
    /// SAS emoji to compare, once keys are exchanged.
    pub emoji: Option<Vec<EmojiDto>>,
    /// SAS decimal triple, the alternative to the emoji.
    pub decimals: Option<[u16; 3]>,
    /// The cancel reason, for a cancelled flow.
    pub cancel_reason: Option<String>,
}

impl From<crate::verification::FlowSummary> for FlowDto {
    fn from(f: crate::verification::FlowSummary) -> Self {
        FlowDto {
            flow_id: f.flow_id,
            user_id: f.target_user_id,
            device_id: f.target_device_id,
            stage: f.stage.into(),
            emoji: f.emoji.map(|pairs| {
                pairs
                    .into_iter()
                    .map(|(symbol, description)| EmojiDto {
                        symbol,
                        description,
                    })
                    .collect()
            }),
            decimals: f.decimals.map(|(a, b, c)| [a, b, c]),
            cancel_reason: f.cancel_reason,
        }
    }
}

/// One page of a room timeline: the events plus the cursor to fetch the next
/// (older) page. `next_cursor` is `null` when the last page has been reached.
#[derive(Debug, Serialize, ToSchema)]
pub struct TimelinePage {
    /// The page of events, newest first.
    pub events: Vec<EventDto>,
    /// Opaque cursor for the next (older) page, or `null` at the end.
    pub next_cursor: Option<String>,
}

/// One hit in the `GET /v1/search` response (M9b): the hydrated event plus its
/// BM25 relevance score. The event is the resolved read-API view (latest edited
/// body, redaction-masked), the same shape every other event endpoint returns.
#[derive(Debug, Serialize, ToSchema)]
pub struct SearchResultDto {
    /// The matching event, hydrated from the store.
    pub event: EventDto,
    /// BM25 relevance score (higher is more relevant).
    pub score: f32,
}

/// One page of search results (M9b): the ranked hits, the total match count
/// across all pages, and the cursor to fetch the next page. `next_cursor` is
/// `null` when the last page has been reached.
#[derive(Debug, Serialize, ToSchema)]
pub struct SearchPage {
    /// The hits on this page, most relevant first.
    pub results: Vec<SearchResultDto>,
    /// Total number of matching events across all pages.
    pub total: usize,
    /// Opaque cursor for the next page, or `null` at the end.
    pub next_cursor: Option<String>,
}

/// Server status (`GET /v1/status`, M10): the backfill engine's disk-space health
/// plus per-account backfill progress, and the running build's identity.
#[derive(Debug, Serialize, ToSchema)]
pub struct StatusDto {
    /// History-backfill engine status.
    pub backfill: BackfillStatusDto,
    /// The running binary's build identity.
    pub build: BuildInfoDto,
    /// Per-account sync-service status.
    pub sync: Vec<AccountSyncStatusDto>,
}

/// The running binary's build identity, mirroring the fields logged in the
/// "axon starting" startup line and reported by `axon -V`.
#[derive(Debug, Serialize, ToSchema)]
pub struct BuildInfoDto {
    pub version: String,
    pub git_hash: String,
    pub profile: String,
    pub build_time: String,
    pub rustc_version: String,
}

impl From<crate::build_info::BuildInfo> for BuildInfoDto {
    fn from(b: crate::build_info::BuildInfo) -> Self {
        BuildInfoDto {
            version: b.version,
            git_hash: b.git_hash,
            profile: b.profile,
            build_time: b.build_time,
            rustc_version: b.rustc_version,
        }
    }
}

/// One account's sync-service status (M10-adjacent). Lets a client — or an
/// operator polling `/v1/status` — tell that sync is actually running rather
/// than silently wedged, instead of only inferring it from the absence of new
/// events.
#[derive(Debug, Serialize, ToSchema)]
pub struct AccountSyncStatusDto {
    pub account_id: Uuid,
    /// One of `"idle"`, `"running"`, `"offline"`, `"terminated"`, `"error"`.
    pub state: String,
    /// When the account last entered `state`, in epoch milliseconds.
    pub since_ms: i64,
}

impl From<crate::sync_status::AccountSyncSnapshot> for AccountSyncStatusDto {
    fn from(s: crate::sync_status::AccountSyncSnapshot) -> Self {
        let since_ms = s
            .since
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        AccountSyncStatusDto {
            account_id: s.account_id,
            state: s.state.to_owned(),
            since_ms,
        }
    }
}

/// The history-backfill engine's status (M10). Backfill grows storage unbounded
/// ("to room start"), so it pauses when free space is low; live sync is never
/// paused.
#[derive(Debug, Serialize, ToSchema)]
pub struct BackfillStatusDto {
    /// Whether backfill is currently paused because free disk space is low.
    pub paused: bool,
    /// Why backfill is paused, or `null` when it is not paused. Currently only
    /// `"low_disk"`.
    pub reason: Option<String>,
    /// Free bytes on the guarded filesystem, read live.
    pub free_bytes: u64,
    /// Per-account backfill progress, so a client can tell whether history backfill
    /// is still running or done.
    pub accounts: Vec<AccountBackfillDto>,
}

/// One account's backfill progress (M10).
#[derive(Debug, Serialize, ToSchema)]
pub struct AccountBackfillDto {
    /// The account.
    pub account_id: Uuid,
    /// Total events stored for the account.
    pub events: i64,
    /// Currently-joined rooms with any stored events.
    pub rooms_total: i64,
    /// Of those, how many are backfilled to the room's start.
    pub rooms_backfilled: i64,
    /// Whether every joined room is fully backfilled (nothing left to fetch).
    pub complete: bool,
}

impl From<axon_store::AccountBackfillProgress> for AccountBackfillDto {
    fn from(p: axon_store::AccountBackfillProgress) -> Self {
        AccountBackfillDto {
            account_id: p.account_id,
            events: p.events_total,
            rooms_total: p.rooms_total,
            rooms_backfilled: p.rooms_backfilled,
            complete: p.rooms_total > 0 && p.rooms_backfilled == p.rooms_total,
        }
    }
}

impl BackfillStatusDto {
    /// Assemble from the disk snapshot (port) and the per-account progress (store).
    pub fn new(
        snapshot: crate::backfill::BackfillStatusSnapshot,
        accounts: Vec<axon_store::AccountBackfillProgress>,
    ) -> Self {
        BackfillStatusDto {
            paused: snapshot.paused_low_disk,
            reason: snapshot.paused_low_disk.then(|| "low_disk".to_owned()),
            free_bytes: snapshot.free_bytes,
            accounts: accounts.into_iter().map(AccountBackfillDto::from).collect(),
        }
    }
}

/// One emoji's tally in the `GET …/events/{event_id}/reactions` response (M8).
/// The response body is a JSON object keyed by emoji — `{ "👍": { … }, "❤️":
/// { … } }` — with this as each value, resolved over the event's reactions
/// regardless of pagination (issue #22 Option A; the store hard-caps a pathological
/// event at the oldest 1000 distinct `(sender, key)` pairs).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ReactionDto {
    /// Distinct senders who reacted with this key (a `(sender, key)` duplicate
    /// counts once).
    pub count: i64,
    /// Whether this Axon account's own user is among the senders.
    pub me: bool,
    /// The distinct Matrix user ids that reacted with this key.
    pub senders: Vec<String>,
    /// The account user's own reaction event ids for this key — the events a
    /// client redacts (`DELETE …/rooms/{room_id}/events/{event_id}`) to withdraw
    /// the reaction. Empty unless `me` is true. Exposed because the collapsed
    /// timeline strips the raw `m.reaction` rows, so a client can no longer
    /// recover these ids by scanning events itself.
    #[serde(default)]
    pub my_event_ids: Vec<String>,
}

impl From<ReactionTally> for ReactionDto {
    fn from(t: ReactionTally) -> Self {
        ReactionDto {
            count: t.count,
            me: t.me,
            senders: t.senders,
            my_event_ids: t.my_event_ids,
        }
    }
}

/// One thread in the `GET …/rooms/{room_id}/threads` response (M8): a thread root
/// with its reply count and latest reply. The root event itself is fetched via
/// the single-event endpoint; the members are paged via the thread timeline.
#[derive(Debug, Serialize, ToSchema)]
pub struct ThreadSummaryDto {
    /// The `event_id` of the thread root (the event the members relate to).
    pub root_event_id: String,
    /// Number of actual-message thread members: redacted members and any
    /// (illegitimate) state-event members are excluded.
    pub reply_count: i64,
    /// The `event_id` of the most recent thread member, or `null` if none.
    pub latest_reply_event_id: Option<String>,
    /// `origin_server_ts` of the most recent member in milliseconds, or `null`.
    pub latest_reply_ts: Option<i64>,
}

impl From<ThreadSummary> for ThreadSummaryDto {
    fn from(s: ThreadSummary) -> Self {
        ThreadSummaryDto {
            root_event_id: s.root_event_id,
            reply_count: s.reply_count,
            latest_reply_event_id: s.latest_reply_event_id,
            latest_reply_ts: s.latest_reply_ts,
        }
    }
}

/// One namespace of per-device client state (M12, ADR 0048), as the merged
/// last-write-wins view across *all* the account's devices: per key, the newest
/// write wins and deleted keys are absent. This is what
/// `GET /v1/devices/{device_id}/state/{namespace}` returns, so a starting
/// client sees the state its sibling devices left.
#[derive(Debug, Serialize, ToSchema)]
pub struct DeviceStateDto {
    /// The namespace read, e.g. `drafts`.
    pub namespace: String,
    /// The merged entries, keyed by the client-chosen key (e.g. a room id).
    pub entries: BTreeMap<String, DeviceStateEntryDto>,
}

/// One winning entry in a merged device-state read (M12).
#[derive(Debug, Serialize, ToSchema)]
pub struct DeviceStateEntryDto {
    /// The opaque value the winning device wrote. Axon never interprets it.
    pub value: Value,
    /// The device that wrote the winning value.
    pub device_id: Uuid,
    /// When the winning value was written (server clock), RFC 3339.
    pub updated_at: String,
}

impl From<DeviceStateRow> for DeviceStateEntryDto {
    fn from(row: DeviceStateRow) -> Self {
        DeviceStateEntryDto {
            // The merged store read drops tombstone winners, so a row here
            // always carries a value.
            value: row.value.unwrap_or(Value::Null),
            device_id: row.device_id,
            updated_at: row.updated_at.to_rfc3339(),
        }
    }
}

/// Body of `PUT /v1/devices/{device_id}/state/{namespace}` (M12): a merge-upsert.
/// Only the keys present are touched — other keys in the namespace are left
/// alone — and a `null` value deletes the key (stored as a tombstone so the
/// deletion wins the cross-device merge).
#[derive(Debug, Deserialize, ToSchema)]
pub struct PutDeviceStateRequest {
    /// The entries to write, keyed by the client-chosen key. `null` deletes.
    pub entries: BTreeMap<String, Option<Value>>,
}

/// Response of `PUT /v1/devices/{device_id}/state/{namespace}` (M12).
#[derive(Debug, Serialize, ToSchema)]
pub struct PutDeviceStateResponse {
    /// When the write landed (server clock), RFC 3339 — the last-write-wins
    /// ordering all devices share.
    pub updated_at: String,
}
