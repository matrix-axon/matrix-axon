//! The outbound-message port the mutation handlers depend on.
//!
//! `axon-api` defines this trait — the capability it *needs* — rather than
//! depending on whatever provides it. The real implementation lives in
//! `axon-sync` (its SDK gateway), adapted onto this port by `axon-server` (the
//! composition root). So this crate stays free of `axon-sync` and `matrix-sdk`:
//! handlers speak only [`MessageSender`] and plain types. This mirrors how the
//! read side stays decoupled via the wire-neutral `LiveEvent` in `axon-core`.
//!
//! Every operation returns the resulting Matrix event id on success; failures
//! are [`SendError`], whose variants map 1:1 to HTTP status in
//! [`response`](crate::response).

use async_trait::async_trait;
use axon_core::{
    CreateRoomRequest, Formatted, MatrixProfile, MediaAttachment, PowerLevelChanges,
    PublicRoomsPage, PublicRoomsQuery, Relation, ResolvedPowerLevels,
};
use uuid::Uuid;

/// What can go wrong issuing a mutation. Deliberately small and HTTP-shaped: the
/// adapter that implements [`MessageSender`] collapses its richer backend error
/// into one of these so the handler layer maps a stable set of statuses.
#[derive(Debug)]
pub enum SendError {
    /// The addressed account or room doesn't exist / isn't joined. → `404`.
    NotFound(String),
    /// The operation isn't permitted (e.g. editing a message the account didn't
    /// author, or a homeserver permission denial). → `403`.
    Forbidden(String),
    /// The account couldn't be brought online (homeserver unreachable, auth
    /// failure). Transient and retryable. → `503`.
    Unavailable(String),
    /// A malformed parameter (e.g. an unparseable room or event id). → `400`.
    Invalid(String),
    /// The upstream homeserver rejected or failed the operation. → `502`.
    Upstream(String),
    /// The call didn't complete within its configured timeout — the
    /// send-mutation defense-in-depth timeout (ADR 0030 decision #4, issue
    /// #241), distinct from [`SendError::Upstream`] because a caller should be
    /// able to tell "the homeserver answered with an error" apart from "we
    /// gave up waiting". → `504`.
    Timeout(String),
}

/// Sends message-like events (message / edit / redact / react) on behalf of an
/// account. Implemented outside this crate; held in [`AppState`](crate::AppState)
/// as `Arc<dyn MessageSender>`.
#[async_trait]
pub trait MessageSender: Send + Sync {
    /// Send a message to a room; returns the new event id. `body` is the
    /// plain-text content; `formatted`, when present, carries the rich-text
    /// rendering (validated at the handler so both its fields are set).
    /// `relation` attaches an `m.relates_to` (reply and/or thread); its default
    /// (no targets) sends a plain, unrelated message.
    async fn send_message(
        &self,
        account_id: Uuid,
        room_id: &str,
        body: &str,
        formatted: Option<Formatted<'_>>,
        relation: Relation<'_>,
    ) -> Result<String, SendError>;

    /// Send a staged media attachment as an `m.image` or `m.file`. The
    /// attachment bytes are already claimed from the staging service; the sender
    /// owns only the Matrix SDK upload/send operation. `caption`, when present,
    /// becomes the media event body, otherwise the filename is the body.
    async fn send_media(
        &self,
        account_id: Uuid,
        room_id: &str,
        attachment: MediaAttachment,
        caption: Option<&str>,
        relation: Relation<'_>,
    ) -> Result<String, SendError>;

    /// Edit an existing message (`m.replace`); returns the replacement event id.
    /// `formatted` sets rich text on the replacement (see [`send_message`]).
    ///
    /// [`send_message`]: MessageSender::send_message
    async fn edit(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        body: &str,
        formatted: Option<Formatted<'_>>,
    ) -> Result<String, SendError>;

    /// Redact an event, optionally with a reason; returns the redaction event id.
    async fn redact(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        reason: Option<&str>,
    ) -> Result<String, SendError>;

    /// React to an event with `key` (an emoji/short string); returns the
    /// reaction event id.
    async fn react(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        key: &str,
    ) -> Result<String, SendError>;
}

/// Sends ephemeral, homeserver-facing signals that have no lasting timeline
/// presence: read receipts and typing notices (ADR 0067, ADR 0068 M19a). Split
/// from [`MessageSender`] because callers treat these as best-effort — a client
/// fire-and-forgets them from its existing read-marker/typing debounce and never
/// surfaces a failure as a user-facing error — whereas a failed message send is
/// never silently swallowed.
#[async_trait]
pub trait EphemeralSender: Send + Sync {
    /// Mark `event_id` read: sets both the public read receipt (`m.read`) and
    /// the private fully-read marker to the same event in one request
    /// (`Room::send_multiple_receipts`), so third-party Matrix clients see the
    /// room as read.
    async fn send_read_receipt(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
    ) -> Result<(), SendError>;

    /// Set (or clear) this account's typing indicator in a room.
    async fn send_typing_notice(
        &self,
        account_id: Uuid,
        room_id: &str,
        typing: bool,
    ) -> Result<(), SendError>;
}

/// What a [`MembershipSender::leave`] call established about the membership.
/// Port-side mirror of `axon_sync::gateway::LeaveOutcome`, the same way
/// [`SendError`] mirrors that crate's `GatewayError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaveOutcome {
    /// The homeserver accepted the leave. The account is no longer a member,
    /// so local bookkeeping for that membership can be torn down.
    Left,
    /// The request stands, but the homeserver's answer did not establish that
    /// the membership is gone. Callers must leave locally-persisted
    /// membership state (notably `room_invites`) in place and let sync
    /// reconcile it — see ADR 0091.
    Unconfirmed,
}

/// Mutates this account's own or another user's membership in an
/// already-joined room (ADR 0068, M19b): `leave`, `forget`, `invite`, `kick`,
/// `ban`, `unban`. Split from [`MessageSender`] because these produce no event
/// id a caller needs back — the resulting `m.room.member` state event round-trips
/// through sync like any other state change — so every method here returns
/// `()` on success, same shape as [`EphemeralSender`]. `leave` is the one
/// exception: it reports a [`LeaveOutcome`], because "the account left" and
/// "the homeserver would not confirm it" must not drive the same local cleanup.
#[async_trait]
pub trait MembershipSender: Send + Sync {
    /// Leave this room (and any predecessor rooms via tombstone, per the SDK),
    /// or decline a pending invite. Must not require a local SDK `Room`
    /// handle — Sliding Sync can evict an invited room while Axon still
    /// lists it (ADR 0091).
    async fn leave(&self, account_id: Uuid, room_id: &str) -> Result<LeaveOutcome, SendError>;

    /// Forget a left or banned-from room, clearing it from the account's room
    /// list. The homeserver rejects forgetting a room this account is still
    /// joined to or invited to.
    async fn forget(&self, account_id: Uuid, room_id: &str) -> Result<(), SendError>;

    /// Invite `user_id` to this room.
    async fn invite(&self, account_id: Uuid, room_id: &str, user_id: &str)
        -> Result<(), SendError>;

    /// Kick `user_id` from this room, optionally with a reason.
    async fn kick(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), SendError>;

    /// Ban `user_id` from this room, optionally with a reason.
    async fn ban(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), SendError>;

    /// Unban `user_id` from this room, optionally with a reason.
    async fn unban(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), SendError>;
}

/// Enters a new room on behalf of an account — join, knock, create, or create
/// a DM (ADR 0068, M19c). Split from [`MembershipSender`] because none of
/// these four resolve through an existing room handle — there is no room yet
/// — and unlike the M19b verbs, every method here returns the resulting
/// room's id rather than `()`.
#[async_trait]
pub trait RoomEntrySender: Send + Sync {
    /// Join a room by id or alias. `server_names` are federation-resolution
    /// hints (ruma's `via`) for an alias/id this account's client has no
    /// direct path to. Returns the joined room's id.
    async fn join(
        &self,
        account_id: Uuid,
        room_id_or_alias: &str,
        server_names: &[String],
    ) -> Result<String, SendError>;

    /// Knock on a room, optionally with a reason. Returns the id of the room
    /// knocked on.
    async fn knock(
        &self,
        account_id: Uuid,
        room_id_or_alias: &str,
        reason: Option<&str>,
        server_names: &[String],
    ) -> Result<String, SendError>;

    /// Create a new room. Returns the created room's id.
    async fn create_room(
        &self,
        account_id: Uuid,
        request: CreateRoomRequest,
    ) -> Result<String, SendError>;

    /// Create a DM with `user_id`. Returns the DM room's id.
    async fn create_dm(&self, account_id: Uuid, user_id: &str) -> Result<String, SendError>;
}

/// Sets this room's `name`/`topic`/`avatar` state and this account's `m.tag`
/// room account data (ADR 0068, M19d). Split from [`MembershipSender`] rather
/// than folded into it because, unlike a membership change, `set_tag`/
/// `remove_tag` write **room account data**, not a state event — there is no
/// `m.room.member`-style state change for the homeserver to fan out to other
/// members, only a private per-user record. All six methods still return
/// `()` on success, same shape as [`MembershipSender`]: none hand back an id
/// a caller needs, since the resulting state/account-data change round-trips
/// through sync like any other.
#[async_trait]
pub trait RoomSettingsSender: Send + Sync {
    /// Set this room's `m.room.name`. An empty `name` clears it — the SDK has
    /// no separate "remove" primitive for name/topic (unlike avatar), so a
    /// blank value is the clear signal.
    async fn set_name(&self, account_id: Uuid, room_id: &str, name: &str) -> Result<(), SendError>;

    /// Set this room's `m.room.topic`. An empty `topic` clears it, same
    /// convention as [`set_name`](Self::set_name).
    async fn set_topic(
        &self,
        account_id: Uuid,
        room_id: &str,
        topic: &str,
    ) -> Result<(), SendError>;

    /// Upload `attachment`'s bytes as this room's avatar and set
    /// `m.room.avatar` to the resulting `mxc://` URI in one call. The
    /// attachment is already claimed from the staging service, mirroring
    /// [`MessageSender::send_media`](crate::sender::MessageSender::send_media);
    /// the sender owns only the Matrix SDK upload/state-event operation.
    async fn set_avatar(
        &self,
        account_id: Uuid,
        room_id: &str,
        attachment: MediaAttachment,
    ) -> Result<(), SendError>;

    /// Clear this room's `m.room.avatar`.
    async fn remove_avatar(&self, account_id: Uuid, room_id: &str) -> Result<(), SendError>;

    /// Add or update `tag` (Matrix's wire form, e.g. `m.favourite`,
    /// `m.lowpriority`, or a `u.`-prefixed custom tag) on this room, with an
    /// optional sort `order`. This is room account data, not a state event —
    /// it's private to this account, not visible to other room members.
    async fn set_tag(
        &self,
        account_id: Uuid,
        room_id: &str,
        tag: &str,
        order: Option<f64>,
    ) -> Result<(), SendError>;

    /// Remove `tag` from this room.
    async fn remove_tag(&self, account_id: Uuid, room_id: &str, tag: &str)
        -> Result<(), SendError>;
}

/// Reads and writes a room's power levels (ADR 0068, M19e): role thresholds
/// (`ban`/`invite`/`kick`/`redact`/`events_default`/`state_default`/
/// `users_default`) and individual per-user levels, merged into one
/// `m.room.power_levels` state event on write. This is the one M19 port
/// whose write can leave the caller permanently unable to self-correct — a
/// homeserver accepts a power-level write that drops the sender's own
/// resolved level below what it takes to send another `m.room.power_levels`
/// event, so `set_power_levels` must reject that outcome itself
/// (`SendError::Invalid`) unless the request explicitly acknowledges it, since
/// there is no `M_FORBIDDEN` for the implementation to translate.
#[async_trait]
pub trait PowerLevelsSender: Send + Sync {
    /// Apply `changes` to this room's power levels in one
    /// `m.room.power_levels` write.
    async fn set_power_levels(
        &self,
        account_id: Uuid,
        room_id: &str,
        changes: PowerLevelChanges,
    ) -> Result<(), SendError>;

    /// Read this room's fully resolved power levels (defaults filled in).
    async fn power_levels(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<ResolvedPowerLevels, SendError>;
}

/// This account's own profile and ignore list, plus reading another user's
/// profile and searching a homeserver's public-room directory (ADR 0068,
/// M19f: `set_display_name`, `set_avatar_url`, `fetch_user_profile_of`,
/// `ignore_user`/`unignore_user`, `public_rooms_filtered`). Kept as one port
/// for the same reason ADR 0068 keeps M19f one PR — none of these resolve
/// through a `Room` handle, all go straight through `ClientManager::get_or_connect`
/// — even though `user_profile` and `public_rooms` are reads, not mutations:
/// [`PowerLevelsSender`] already mixes a write and a read in one trait for
/// the same "same resolution path" reason.
#[async_trait]
pub trait AccountActionsSender: Send + Sync {
    /// Set this account's own display name. An empty `display_name` clears
    /// it (mirroring [`RoomSettingsSender::set_name`]'s convention), issuing
    /// the SDK's real profile-field-delete call rather than writing an empty
    /// string to the homeserver.
    async fn set_display_name(&self, account_id: Uuid, display_name: &str)
        -> Result<(), SendError>;

    /// Upload `attachment`'s bytes as this account's avatar and set the
    /// account's profile avatar to the resulting `mxc://` URI in one call,
    /// mirroring [`RoomSettingsSender::set_avatar`]'s staged-upload shape.
    async fn set_avatar(
        &self,
        account_id: Uuid,
        attachment: MediaAttachment,
    ) -> Result<(), SendError>;

    /// Clear this account's profile avatar.
    async fn remove_avatar(&self, account_id: Uuid) -> Result<(), SendError>;

    /// Read `user_id`'s Matrix profile (display name + avatar). `user_id` may
    /// be this account's own user id or any other user's.
    async fn user_profile(
        &self,
        account_id: Uuid,
        user_id: &str,
    ) -> Result<MatrixProfile, SendError>;

    /// Add `user_id` to this account's ignore list.
    async fn ignore_user(&self, account_id: Uuid, user_id: &str) -> Result<(), SendError>;

    /// Remove `user_id` from this account's ignore list.
    async fn unignore_user(&self, account_id: Uuid, user_id: &str) -> Result<(), SendError>;

    /// Search a homeserver's public-room directory. A paginated **read**,
    /// unlike every other method on this trait — see [`PublicRoomsQuery`]/
    /// [`PublicRoomsPage`].
    async fn public_rooms(
        &self,
        account_id: Uuid,
        query: PublicRoomsQuery,
    ) -> Result<PublicRoomsPage, SendError>;
}
