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
    ephemeral_send_timeout: Duration,
    membership_mutation_timeout: Duration,
    room_entry_timeout: Duration,
}

impl GatewayAdapter {
    pub fn new(
        gateway: SdkGateway,
        send_mutation_timeout: Duration,
        upstream_upload_timeout: Duration,
        ephemeral_send_timeout: Duration,
        membership_mutation_timeout: Duration,
        room_entry_timeout: Duration,
    ) -> Self {
        Self {
            gateway,
            send_mutation_timeout,
            upstream_upload_timeout,
            ephemeral_send_timeout,
            membership_mutation_timeout,
            room_entry_timeout,
        }
    }

    /// Runs `fut` under `timeout`, collapsing a timeout into the same
    /// [`timed_out`] shape every other site in this file builds by hand, and
    /// mapping a gateway error via [`map_err`]. Used by the
    /// [`AccountActionsSender`] impl below, whose seven methods would
    /// otherwise repeat this file's established
    /// `timeout(...).await.map_err(timed_out)?.map_err(map_err)` shape a
    /// seventh time in a row; the five earlier trait impls in this file
    /// predate this helper and are left as-is rather than folded in as part
    /// of an unrelated refactor.
    async fn with_timeout<T>(
        &self,
        timeout: Duration,
        verb: &str,
        fut: impl Future<Output = Result<T, GatewayError>>,
    ) -> Result<T, SendError> {
        tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| timed_out(verb, timeout))?
            .map_err(map_err)
    }

    /// Same shape as [`with_timeout`](Self::with_timeout), but always runs
    /// under [`send_mutation_timeout`](Self::send_mutation_timeout) and maps a
    /// timeout via [`mutation_timed_out`] (a `504`, ADR 0030 decision #4,
    /// issue #241) instead of [`timed_out`]'s `502`. Used by the four
    /// [`MessageSender`] methods below.
    async fn with_mutation_timeout<T>(
        &self,
        verb: &str,
        fut: impl Future<Output = Result<T, GatewayError>>,
    ) -> Result<T, SendError> {
        tokio::time::timeout(self.send_mutation_timeout, fut)
            .await
            .map_err(|_| mutation_timed_out(verb, self.send_mutation_timeout))?
            .map_err(map_err)
    }
}

/// The `SendError::Upstream` an outbound call reports when it hits its
/// configured per-call `timeout` before the homeserver responds. Shared by
/// every timeout site in this adapter (media upload, ephemeral sends,
/// membership mutations, room-entry mutations) so the message format has one
/// definition instead of one per port.
fn timed_out(verb: &str, timeout: Duration) -> SendError {
    SendError::Upstream(format!("{verb} timed out after {}s", timeout.as_secs()))
}

/// The [`SendError::Timeout`] a `send`/`edit`/`redact`/`react` call reports
/// when it hits [`GatewayAdapter::send_mutation_timeout`] (ADR 0030 decision
/// #4, issue #241) — a `504`, distinct from [`timed_out`]'s `502`, since the
/// SDK call is still outstanding rather than having received an error
/// response.
fn mutation_timed_out(verb: &str, timeout: Duration) -> SendError {
    SendError::Timeout(format!("{verb} timed out after {}s", timeout.as_secs()))
}

/// Map a sync-layer gateway error onto the API-layer send error (and thus an
/// HTTP status): unknown account / room → not found, a failed connect →
/// unavailable, bad input → invalid, a homeserver failure → upstream.
fn map_err(err: GatewayError) -> SendError {
    match err {
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
    }
}

#[async_trait]
impl MessageSender for GatewayAdapter {
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

    async fn send_media(
        &self,
        account_id: Uuid,
        room_id: &str,
        attachment: MediaAttachment,
        caption: Option<&str>,
        relation: Relation<'_>,
    ) -> Result<String, SendError> {
        tokio::time::timeout(
            self.upstream_upload_timeout,
            self.gateway
                .send_media(account_id, room_id, attachment, caption, relation),
        )
        .await
        .map_err(|_| timed_out("media send", self.upstream_upload_timeout))?
        .map_err(map_err)
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
}

#[async_trait]
impl EphemeralSender for GatewayAdapter {
    async fn send_read_receipt(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
    ) -> Result<(), SendError> {
        tokio::time::timeout(
            self.ephemeral_send_timeout,
            self.gateway
                .send_read_receipt(account_id, room_id, event_id),
        )
        .await
        .map_err(|_| timed_out("read receipt", self.ephemeral_send_timeout))?
        .map_err(map_err)
    }

    async fn send_typing_notice(
        &self,
        account_id: Uuid,
        room_id: &str,
        typing: bool,
    ) -> Result<(), SendError> {
        tokio::time::timeout(
            self.ephemeral_send_timeout,
            self.gateway.send_typing_notice(account_id, room_id, typing),
        )
        .await
        .map_err(|_| timed_out("typing notice", self.ephemeral_send_timeout))?
        .map_err(map_err)
    }
}

#[async_trait]
impl MembershipSender for GatewayAdapter {
    async fn leave(&self, account_id: Uuid, room_id: &str) -> Result<LeaveOutcome, SendError> {
        tokio::time::timeout(
            self.membership_mutation_timeout,
            self.gateway.leave(account_id, room_id),
        )
        .await
        .map_err(|_| timed_out("leave", self.membership_mutation_timeout))?
        .map(|outcome| match outcome {
            GatewayLeaveOutcome::Left => LeaveOutcome::Left,
            GatewayLeaveOutcome::Gone => LeaveOutcome::Gone,
            GatewayLeaveOutcome::Unconfirmed => LeaveOutcome::Unconfirmed,
        })
        .map_err(map_err)
    }

    async fn forget(&self, account_id: Uuid, room_id: &str) -> Result<(), SendError> {
        tokio::time::timeout(
            self.membership_mutation_timeout,
            self.gateway.forget(account_id, room_id),
        )
        .await
        .map_err(|_| timed_out("forget", self.membership_mutation_timeout))?
        .map_err(map_err)
    }

    async fn invite(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
    ) -> Result<(), SendError> {
        tokio::time::timeout(
            self.membership_mutation_timeout,
            self.gateway.invite(account_id, room_id, user_id),
        )
        .await
        .map_err(|_| timed_out("invite", self.membership_mutation_timeout))?
        .map_err(map_err)
    }

    async fn kick(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), SendError> {
        tokio::time::timeout(
            self.membership_mutation_timeout,
            self.gateway.kick(account_id, room_id, user_id, reason),
        )
        .await
        .map_err(|_| timed_out("kick", self.membership_mutation_timeout))?
        .map_err(map_err)
    }

    async fn ban(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), SendError> {
        tokio::time::timeout(
            self.membership_mutation_timeout,
            self.gateway.ban(account_id, room_id, user_id, reason),
        )
        .await
        .map_err(|_| timed_out("ban", self.membership_mutation_timeout))?
        .map_err(map_err)
    }

    async fn unban(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), SendError> {
        tokio::time::timeout(
            self.membership_mutation_timeout,
            self.gateway.unban(account_id, room_id, user_id, reason),
        )
        .await
        .map_err(|_| timed_out("unban", self.membership_mutation_timeout))?
        .map_err(map_err)
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
        tokio::time::timeout(
            self.room_entry_timeout,
            self.gateway
                .join(account_id, room_id_or_alias, server_names),
        )
        .await
        .map_err(|_| timed_out("join", self.room_entry_timeout))?
        .map_err(map_err)
    }

    async fn knock(
        &self,
        account_id: Uuid,
        room_id_or_alias: &str,
        reason: Option<&str>,
        server_names: &[String],
    ) -> Result<String, SendError> {
        tokio::time::timeout(
            self.room_entry_timeout,
            self.gateway
                .knock(account_id, room_id_or_alias, reason, server_names),
        )
        .await
        .map_err(|_| timed_out("knock", self.room_entry_timeout))?
        .map_err(map_err)
    }

    async fn create_room(
        &self,
        account_id: Uuid,
        request: CreateRoomRequest,
    ) -> Result<String, SendError> {
        tokio::time::timeout(
            self.room_entry_timeout,
            self.gateway.create_room(account_id, request),
        )
        .await
        .map_err(|_| timed_out("create_room", self.room_entry_timeout))?
        .map_err(map_err)
    }

    async fn create_dm(&self, account_id: Uuid, user_id: &str) -> Result<String, SendError> {
        tokio::time::timeout(
            self.room_entry_timeout,
            self.gateway.create_dm(account_id, user_id),
        )
        .await
        .map_err(|_| timed_out("create_dm", self.room_entry_timeout))?
        .map_err(map_err)
    }
}

#[async_trait]
impl RoomSettingsSender for GatewayAdapter {
    async fn set_name(&self, account_id: Uuid, room_id: &str, name: &str) -> Result<(), SendError> {
        tokio::time::timeout(
            self.membership_mutation_timeout,
            self.gateway.set_name(account_id, room_id, name),
        )
        .await
        .map_err(|_| timed_out("set_name", self.membership_mutation_timeout))?
        .map_err(map_err)
    }

    async fn set_topic(
        &self,
        account_id: Uuid,
        room_id: &str,
        topic: &str,
    ) -> Result<(), SendError> {
        tokio::time::timeout(
            self.membership_mutation_timeout,
            self.gateway.set_topic(account_id, room_id, topic),
        )
        .await
        .map_err(|_| timed_out("set_topic", self.membership_mutation_timeout))?
        .map_err(map_err)
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
        tokio::time::timeout(
            self.upstream_upload_timeout,
            self.gateway.set_avatar(account_id, room_id, attachment),
        )
        .await
        .map_err(|_| timed_out("set_avatar", self.upstream_upload_timeout))?
        .map_err(map_err)
    }

    async fn remove_avatar(&self, account_id: Uuid, room_id: &str) -> Result<(), SendError> {
        tokio::time::timeout(
            self.membership_mutation_timeout,
            self.gateway.remove_avatar(account_id, room_id),
        )
        .await
        .map_err(|_| timed_out("remove_avatar", self.membership_mutation_timeout))?
        .map_err(map_err)
    }

    async fn set_tag(
        &self,
        account_id: Uuid,
        room_id: &str,
        tag: &str,
        order: Option<f64>,
    ) -> Result<(), SendError> {
        tokio::time::timeout(
            self.membership_mutation_timeout,
            self.gateway.set_tag(account_id, room_id, tag, order),
        )
        .await
        .map_err(|_| timed_out("set_tag", self.membership_mutation_timeout))?
        .map_err(map_err)
    }

    async fn remove_tag(
        &self,
        account_id: Uuid,
        room_id: &str,
        tag: &str,
    ) -> Result<(), SendError> {
        tokio::time::timeout(
            self.membership_mutation_timeout,
            self.gateway.remove_tag(account_id, room_id, tag),
        )
        .await
        .map_err(|_| timed_out("remove_tag", self.membership_mutation_timeout))?
        .map_err(map_err)
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
        tokio::time::timeout(
            self.membership_mutation_timeout,
            self.gateway.set_power_levels(account_id, room_id, changes),
        )
        .await
        .map_err(|_| timed_out("set_power_levels", self.membership_mutation_timeout))?
        .map_err(map_err)
    }

    async fn power_levels(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<ResolvedPowerLevels, SendError> {
        tokio::time::timeout(
            self.membership_mutation_timeout,
            self.gateway.power_levels(account_id, room_id),
        )
        .await
        .map_err(|_| timed_out("power_levels", self.membership_mutation_timeout))?
        .map_err(map_err)
    }
}

#[async_trait]
impl AccountActionsSender for GatewayAdapter {
    async fn set_display_name(
        &self,
        account_id: Uuid,
        display_name: &str,
    ) -> Result<(), SendError> {
        self.with_timeout(
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
        self.with_timeout(
            self.upstream_upload_timeout,
            "set_avatar",
            self.gateway.set_account_avatar(account_id, attachment),
        )
        .await
    }

    async fn remove_avatar(&self, account_id: Uuid) -> Result<(), SendError> {
        self.with_timeout(
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
        self.with_timeout(
            self.room_entry_timeout,
            "user_profile",
            self.gateway.fetch_user_profile_of(account_id, user_id),
        )
        .await
    }

    async fn ignore_user(&self, account_id: Uuid, user_id: &str) -> Result<(), SendError> {
        self.with_timeout(
            self.membership_mutation_timeout,
            "ignore_user",
            self.gateway.ignore_user(account_id, user_id),
        )
        .await
    }

    async fn unignore_user(&self, account_id: Uuid, user_id: &str) -> Result<(), SendError> {
        self.with_timeout(
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
        self.with_timeout(
            self.room_entry_timeout,
            "public_rooms",
            self.gateway.public_rooms(account_id, query),
        )
        .await
    }
}
