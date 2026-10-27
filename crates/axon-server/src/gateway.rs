//! Composition-root adapter: binds `axon-sync`'s concrete [`SdkGateway`] to
//! `axon-api`'s [`MessageSender`], [`EphemeralSender`], [`MembershipSender`],
//! [`RoomEntrySender`], [`RoomSettingsSender`], [`PowerLevelsSender`], and
//! [`AccountActionsSender`] ports.
//!
//! `axon-api` and `axon-sync` never depend on each other; this binary is the one
//! place that knows both, so the adapter lives here. It delegates each call to
//! the SDK gateway and maps the sync-layer [`GatewayError`] onto the API-layer
//! [`SendError`] — a mechanical 1:1 translation, the small cost of keeping the
//! two crates decoupled.

use std::future::Future;
use std::time::Duration;

use async_trait::async_trait;
use axon_api::{
    AccountActionsSender, EphemeralSender, Formatted, LeaveOutcome, MediaAttachment,
    MembershipSender, MessageSender, PowerLevelsSender, Relation, RoomEntrySender,
    RoomSettingsSender, SendError,
};
use axon_core::{
    CreateRoomRequest, MatrixProfile, PowerLevelChanges, PublicRoomsPage, PublicRoomsQuery,
    ResolvedPowerLevels,
};
use axon_sync::{GatewayError, GatewayLeaveOutcome, SdkGateway};
use uuid::Uuid;

/// Wraps the sync engine's gateway so it satisfies the API's `MessageSender`
/// port. The orphan rule requires a local newtype to carry the impl.
pub struct GatewayAdapter {
    gateway: SdkGateway,
    /// Per-call timeout for `send`/`edit`/`redact`/`react` (ADR 0030 decision
    /// #4, issue #241) — see [`MessageSender`]'s impl below.
    send_mutation_timeout: Duration,
    upstream_upload_timeout: Duration,
    /// Read receipts are durable state, so they get longer than typing
    /// notices, which expire on their own (issue #627).
    read_receipt_timeout: Duration,
    typing_timeout: Duration,
    membership_mutation_timeout: Duration,
    /// Also bounds `leave` and `forget`: leaving a large room needs state
    /// resolution, just as joining one needs federation (ADR 0121 decision 5).
    room_entry_timeout: Duration,
}

/// Every timeout [`GatewayAdapter`] applies, one per kind of call.
pub struct GatewayTimeouts {
    pub send_mutation: Duration,
    pub upstream_upload: Duration,
    pub read_receipt: Duration,
    pub typing: Duration,
    pub membership_mutation: Duration,
    pub room_entry: Duration,
}

impl GatewayAdapter {
    pub fn new(gateway: SdkGateway, timeouts: GatewayTimeouts) -> Self {
        Self {
            gateway,
            send_mutation_timeout: timeouts.send_mutation,
            upstream_upload_timeout: timeouts.upstream_upload,
            read_receipt_timeout: timeouts.read_receipt,
            typing_timeout: timeouts.typing,
            membership_mutation_timeout: timeouts.membership_mutation,
            room_entry_timeout: timeouts.room_entry,
        }
    }

    /// [`bounded`] under [`send_mutation_timeout`](Self::send_mutation_timeout).
    /// Used by the [`MessageSender`] methods below.
    async fn with_mutation_timeout<T>(
        &self,
        verb: &str,
        fut: impl Future<Output = Result<T, GatewayError>>,
    ) -> Result<T, SendError> {
        bounded(self.send_mutation_timeout, verb, fut).await
    }
}

/// Runs `fut` under `timeout`, reporting a timeout via [`timed_out`] and
/// mapping a gateway error via [`map_err`]. Every outbound call in this
/// adapter goes through it, so none can report a timeout any other way.
async fn bounded<T>(
    timeout: Duration,
    verb: &str,
    fut: impl Future<Output = Result<T, GatewayError>>,
) -> Result<T, SendError> {
    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| timed_out(verb, timeout))?
        .map_err(map_err)
}

/// The [`SendError::Timeout`] (a `504`) an outbound call reports when it hits
/// its timeout before the homeserver answers (ADR 0121 decision 5, extending
/// ADR 0030 decision #4 beyond sends). Dropping the SDK call ends Axon's wait,
/// not the homeserver's work, so the call may still take effect; a `502` is
/// kept for a homeserver that answered with an error.
fn timed_out(verb: &str, timeout: Duration) -> SendError {
    SendError::Timeout(format!("{verb} timed out after {}s", timeout.as_secs()))
}

/// Map a sync-layer gateway error onto the API-layer send error (and thus an
/// HTTP status): unknown account / room → not found, a failed connect →
/// unavailable, bad input → invalid, a homeserver failure → upstream.
fn map_err(err: GatewayError) -> SendError {
    match err {
        GatewayError::EventTooLarge => SendError::EventTooLarge,
        GatewayError::UnknownAccount(id) => SendError::NotFound(format!("no such account: {id}")),
        GatewayError::AccountNotActive(id) => {
            SendError::Forbidden(format!("account not active: {id}"))
        }
        GatewayError::RoomNotFound(room) => SendError::NotFound(format!("room not found: {room}")),
        GatewayError::MediaNotFound(media) => {
            SendError::NotFound(format!("media not found: {media}"))
        }
        GatewayError::Forbidden(msg) => SendError::Forbidden(msg),
        GatewayError::NotConnected(msg) => SendError::Unavailable(msg),
        GatewayError::Invalid(msg) => SendError::Invalid(msg),
        GatewayError::Upstream(msg) => SendError::Upstream(msg),
        GatewayError::Timeout(msg) => SendError::Timeout(msg),
        // Unreachable on this path: mutations upload attachments, they never
        // download and decrypt one, so only the media proxy can produce this.
        // Folded onto `Upstream` rather than asserted away, so a future caller
        // that does reach it degrades to the previous behaviour instead of
        // panicking in production.
        GatewayError::Undecryptable(msg) => SendError::Upstream(msg),
    }
}

#[async_trait]
impl MessageSender for GatewayAdapter {
    async fn send_with_previews(
        &self,
        account_id: Uuid,
        room_id: &str,
        message: axon_core::link_preview::PreviewSend<'_>,
        attachment: Option<MediaAttachment>,
    ) -> Result<String, SendError> {
        bounded(
            self.upstream_upload_timeout,
            "preview send",
            self.gateway
                .send_with_previews(account_id, room_id, message, attachment),
        )
        .await
    }
    async fn send_message(
        &self,
        account_id: Uuid,
        room_id: &str,
        body: &str,
        formatted: Option<Formatted<'_>>,
        relation: Relation<'_>,
    ) -> Result<String, SendError> {
        self.with_mutation_timeout(
            "send",
            self.gateway
                .send_message(account_id, room_id, body, formatted, relation),
        )
        .await
    }

    async fn send_sticker(
        &self,
        account_id: Uuid,
        room_id: &str,
        sticker: &axon_core::image_pack::Sticker,
        relation: Relation<'_>,
    ) -> Result<String, SendError> {
        self.with_mutation_timeout(
            "sticker send",
            self.gateway
                .send_sticker(account_id, room_id, sticker, relation),
        )
        .await
    }

    async fn send_media(
        &self,
        account_id: Uuid,
        room_id: &str,
        attachment: MediaAttachment,
        caption: Option<&str>,
        formatted: Option<Formatted<'_>>,
        relation: Relation<'_>,
    ) -> Result<String, SendError> {
        bounded(
            self.upstream_upload_timeout,
            "media send",
            self.gateway.send_media(
                account_id, room_id, attachment, caption, formatted, relation,
            ),
        )
        .await
    }

    async fn edit(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        body: &str,
        formatted: Option<Formatted<'_>>,
    ) -> Result<String, SendError> {
        self.with_mutation_timeout(
            "edit",
            self.gateway
                .edit(account_id, room_id, event_id, body, formatted),
        )
        .await
    }

    async fn redact(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        reason: Option<&str>,
    ) -> Result<String, SendError> {
        self.with_mutation_timeout(
            "redact",
            self.gateway.redact(account_id, room_id, event_id, reason),
        )
        .await
    }

    async fn react(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        key: &str,
    ) -> Result<String, SendError> {
        self.with_mutation_timeout(
            "react",
            self.gateway.react(account_id, room_id, event_id, key),
        )
        .await
    }

    async fn react_with_shortcode(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        key: &str,
        shortcode: Option<&str>,
    ) -> Result<String, SendError> {
        self.with_mutation_timeout(
            "react",
            self.gateway
                .react_with_shortcode(account_id, room_id, event_id, key, shortcode),
        )
        .await
    }
}

#[async_trait]
impl EphemeralSender for GatewayAdapter {
    async fn send_read_receipt(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
    ) -> Result<(), SendError> {
        bounded(
            self.read_receipt_timeout,
            "read receipt",
            self.gateway
                .send_read_receipt(account_id, room_id, event_id),
        )
        .await
    }

    async fn send_typing_notice(
        &self,
        account_id: Uuid,
        room_id: &str,
        typing: bool,
    ) -> Result<(), SendError> {
        bounded(
            self.typing_timeout,
            "typing notice",
            self.gateway.send_typing_notice(account_id, room_id, typing),
        )
        .await
    }
}

#[async_trait]
impl MembershipSender for GatewayAdapter {
    async fn leave(&self, account_id: Uuid, room_id: &str) -> Result<LeaveOutcome, SendError> {
        bounded(
            self.room_entry_timeout,
            "leave",
            self.gateway.leave(account_id, room_id),
        )
        .await
        .map(|outcome| match outcome {
            GatewayLeaveOutcome::Left => LeaveOutcome::Left,
            GatewayLeaveOutcome::Gone => LeaveOutcome::Gone,
            GatewayLeaveOutcome::Unconfirmed => LeaveOutcome::Unconfirmed,
        })
    }

    async fn forget(&self, account_id: Uuid, room_id: &str) -> Result<(), SendError> {
        bounded(
            self.room_entry_timeout,
            "forget",
            self.gateway.forget(account_id, room_id),
        )
        .await
    }

    async fn invite(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
    ) -> Result<(), SendError> {
        bounded(
            self.membership_mutation_timeout,
            "invite",
            self.gateway.invite(account_id, room_id, user_id),
        )
        .await
    }

    async fn kick(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), SendError> {
        bounded(
            self.membership_mutation_timeout,
            "kick",
            self.gateway.kick(account_id, room_id, user_id, reason),
        )
        .await
    }

    async fn ban(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), SendError> {
        bounded(
            self.membership_mutation_timeout,
            "ban",
            self.gateway.ban(account_id, room_id, user_id, reason),
        )
        .await
    }

    async fn unban(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), SendError> {
        bounded(
            self.membership_mutation_timeout,
            "unban",
            self.gateway.unban(account_id, room_id, user_id, reason),
        )
        .await
    }
}

#[async_trait]
impl RoomEntrySender for GatewayAdapter {
    async fn join(
        &self,
        account_id: Uuid,
        room_id_or_alias: &str,
        server_names: &[String],
    ) -> Result<String, SendError> {
        bounded(
            self.room_entry_timeout,
            "join",
            self.gateway
                .join(account_id, room_id_or_alias, server_names),
        )
        .await
    }

    async fn knock(
        &self,
        account_id: Uuid,
        room_id_or_alias: &str,
        reason: Option<&str>,
        server_names: &[String],
    ) -> Result<String, SendError> {
        bounded(
            self.room_entry_timeout,
            "knock",
            self.gateway
                .knock(account_id, room_id_or_alias, reason, server_names),
        )
        .await
    }

    async fn create_room(
        &self,
        account_id: Uuid,
        request: CreateRoomRequest,
    ) -> Result<String, SendError> {
        bounded(
            self.room_entry_timeout,
            "create_room",
            self.gateway.create_room(account_id, request),
        )
        .await
    }

    async fn create_dm(&self, account_id: Uuid, user_id: &str) -> Result<String, SendError> {
        bounded(
            self.room_entry_timeout,
            "create_dm",
            self.gateway.create_dm(account_id, user_id),
        )
        .await
    }
}

#[async_trait]
impl RoomSettingsSender for GatewayAdapter {
    async fn set_name(&self, account_id: Uuid, room_id: &str, name: &str) -> Result<(), SendError> {
        bounded(
            self.membership_mutation_timeout,
            "set_name",
            self.gateway.set_name(account_id, room_id, name),
        )
        .await
    }

    async fn set_topic(
        &self,
        account_id: Uuid,
        room_id: &str,
        topic: &str,
    ) -> Result<(), SendError> {
        bounded(
            self.membership_mutation_timeout,
            "set_topic",
            self.gateway.set_topic(account_id, room_id, topic),
        )
        .await
    }

    async fn set_avatar(
        &self,
        account_id: Uuid,
        room_id: &str,
        attachment: MediaAttachment,
    ) -> Result<(), SendError> {
        // Reuses the media-upload timeout (not `membership_mutation_timeout`)
        // since this call, unlike the other five, does a real upload to the
        // homeserver — the same reason `send_media` uses it.
        bounded(
            self.upstream_upload_timeout,
            "set_avatar",
            self.gateway.set_avatar(account_id, room_id, attachment),
        )
        .await
    }

    async fn remove_avatar(&self, account_id: Uuid, room_id: &str) -> Result<(), SendError> {
        bounded(
            self.membership_mutation_timeout,
            "remove_avatar",
            self.gateway.remove_avatar(account_id, room_id),
        )
        .await
    }

    async fn set_tag(
        &self,
        account_id: Uuid,
        room_id: &str,
        tag: &str,
        order: Option<f64>,
    ) -> Result<(), SendError> {
        bounded(
            self.membership_mutation_timeout,
            "set_tag",
            self.gateway.set_tag(account_id, room_id, tag, order),
        )
        .await
    }

    async fn remove_tag(
        &self,
        account_id: Uuid,
        room_id: &str,
        tag: &str,
    ) -> Result<(), SendError> {
        bounded(
            self.membership_mutation_timeout,
            "remove_tag",
            self.gateway.remove_tag(account_id, room_id, tag),
        )
        .await
    }
}

#[async_trait]
impl PowerLevelsSender for GatewayAdapter {
    async fn set_power_levels(
        &self,
        account_id: Uuid,
        room_id: &str,
        changes: PowerLevelChanges,
    ) -> Result<(), SendError> {
        bounded(
            self.membership_mutation_timeout,
            "set_power_levels",
            self.gateway.set_power_levels(account_id, room_id, changes),
        )
        .await
    }

    async fn power_levels(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<ResolvedPowerLevels, SendError> {
        bounded(
            self.membership_mutation_timeout,
            "power_levels",
            self.gateway.power_levels(account_id, room_id),
        )
        .await
    }
}

#[async_trait]
impl AccountActionsSender for GatewayAdapter {
    async fn set_display_name(
        &self,
        account_id: Uuid,
        display_name: &str,
    ) -> Result<(), SendError> {
        bounded(
            self.membership_mutation_timeout,
            "set_display_name",
            self.gateway.set_display_name(account_id, display_name),
        )
        .await
    }

    async fn set_avatar(
        &self,
        account_id: Uuid,
        attachment: MediaAttachment,
    ) -> Result<(), SendError> {
        // Reuses the media-upload timeout (not `membership_mutation_timeout`)
        // since this call does a real upload to the homeserver — the same
        // reason `RoomSettingsSender::set_avatar` uses it.
        bounded(
            self.upstream_upload_timeout,
            "set_avatar",
            self.gateway.set_account_avatar(account_id, attachment),
        )
        .await
    }

    async fn remove_avatar(&self, account_id: Uuid) -> Result<(), SendError> {
        bounded(
            self.membership_mutation_timeout,
            "remove_avatar",
            self.gateway.remove_account_avatar(account_id),
        )
        .await
    }

    async fn user_profile(
        &self,
        account_id: Uuid,
        user_id: &str,
    ) -> Result<MatrixProfile, SendError> {
        // Reuses `room_entry_timeout` (not `membership_mutation_timeout`):
        // `user_id` may be any Matrix user, not just one local to this
        // account's homeserver, and the C-S spec has the homeserver proxy a
        // non-local profile lookup over federation — the same
        // federation-resolution risk `public_rooms` below is given the
        // longer budget for.
        bounded(
            self.room_entry_timeout,
            "user_profile",
            self.gateway.fetch_user_profile_of(account_id, user_id),
        )
        .await
    }

    async fn ignore_user(&self, account_id: Uuid, user_id: &str) -> Result<(), SendError> {
        bounded(
            self.membership_mutation_timeout,
            "ignore_user",
            self.gateway.ignore_user(account_id, user_id),
        )
        .await
    }

    async fn unignore_user(&self, account_id: Uuid, user_id: &str) -> Result<(), SendError> {
        bounded(
            self.membership_mutation_timeout,
            "unignore_user",
            self.gateway.unignore_user(account_id, user_id),
        )
        .await
    }

    async fn public_rooms(
        &self,
        account_id: Uuid,
        query: PublicRoomsQuery,
    ) -> Result<PublicRoomsPage, SendError> {
        // Reuses `room_entry_timeout` (not `membership_mutation_timeout`):
        // like join/knock, a directory search can involve federation
        // resolution — `query.server` explicitly lets a caller target
        // another homeserver's directory rather than a purely local read.
        bounded(
            self.room_entry_timeout,
            "public_rooms",
            self.gateway.public_rooms(account_id, query),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_call_the_homeserver_never_answers_is_a_504_naming_the_verb() {
        let result: Result<(), SendError> =
            bounded(Duration::from_secs(30), "leave", std::future::pending()).await;
        match result {
            Err(SendError::Timeout(message)) => assert_eq!(message, "leave timed out after 30s"),
            other => panic!("expected a timeout, got {other:?}"),
        }
    }

    /// The SDK's own request timeout (30 s by default) fires inside a longer
    /// adapter deadline, so the sync layer's typed timeout must come out as a
    /// `504` as well.
    #[tokio::test(start_paused = true)]
    async fn an_sdk_timeout_inside_the_deadline_is_a_504_too() {
        let result: Result<(), SendError> = bounded(Duration::from_secs(45), "leave", async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Err(GatewayError::Timeout("operation timed out".to_owned()))
        })
        .await;
        match result {
            Err(SendError::Timeout(message)) => assert_eq!(message, "operation timed out"),
            other => panic!("expected a timeout, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_homeserver_error_inside_the_timeout_stays_a_502() {
        let result: Result<(), SendError> = bounded(Duration::from_secs(30), "leave", async {
            Err(GatewayError::Upstream("M_UNKNOWN".to_owned()))
        })
        .await;
        match result {
            Err(SendError::Upstream(message)) => assert_eq!(message, "M_UNKNOWN"),
            other => panic!("expected an upstream error, got {other:?}"),
        }
    }
}
