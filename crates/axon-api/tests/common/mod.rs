//! Shared test doubles for the API integration tests.
//!
//! [`StubSender`] is an in-memory [`MessageSender`] that records the calls it
//! receives and returns a preset outcome, so the mutation handlers can be
//! exercised (routing, request decoding, error mapping) without a real
//! homeserver or sync engine.

#![allow(dead_code)] // each tests/*.rs is its own crate; not all use every helper.

pub mod oidc;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axon_api::{
    AccountActionsSender, AccountLifecycle, ApiError, CurrentTrust, DeleteError, DeviceInfo,
    DeviceList, DeviceListError, DeviceListService, EphemeralSender, FlowStage, FlowSummary,
    Formatted, LeaveOutcome, LoginError, LogoutError, MediaAttachment, MediaError, MediaProxy,
    MediaResource, MemberProfile, MemberProfileError, MemberProfileService, MembershipSender,
    MessageSender, PowerLevelsSender, RecoverError, RedecryptUtdsError, RedecryptUtdsStats,
    Relation, RoomEntrySender, RoomSettingsSender, SearchHit, SearchHits, SearchQuery,
    SearchQueryError, SearchQueryParams, SendError, SenderTrustService, StageUploadError,
    StageUploadRequest, StagedUpload, StagedUploadService, SyncStateProvider, TokenVerifier,
    TrustBundle, TrustError, TrustSnapshot, UploadStream, VerificationService, VerifyError,
};
use axon_core::{
    CreateRoomRequest, MatrixProfile, PowerLevelChanges, PublicRoomsPage, PublicRoomsQuery,
    ResolvedPowerLevels,
};
use futures_util::StreamExt;
use uuid::Uuid;

/// The bearer token the test router's [`StubTokenVerifier`] accepts. Helpers
/// attach it as `Authorization: Bearer {TEST_TOKEN}` so the auth gate (M7b) lets
/// functional requests through; auth-specific tests send a wrong token or none.
pub const TEST_TOKEN: &str = "axon_test-token";

/// An in-memory [`TokenVerifier`] for tests: accepts exactly one token string
/// (while "active") and rejects everything else, so the `/v1/` auth gate can be
/// exercised without a `tokens` row. The real DB-backed `StoreTokenVerifier` is
/// covered by the axon-store token tests.
///
/// The shared `active` flag lets a test simulate out-of-process revocation of a
/// live token: grab [`revocation_handle`](Self::revocation_handle) before wrapping
/// the stub in an `Arc`, then flip it to `false` to make `verify` start rejecting.
pub struct StubTokenVerifier {
    accepted: String,
    active: Arc<AtomicBool>,
}

impl StubTokenVerifier {
    /// A verifier accepting [`TEST_TOKEN`] — the default for functional tests.
    pub fn ok() -> Self {
        Self {
            accepted: TEST_TOKEN.to_owned(),
            active: Arc::new(AtomicBool::new(true)),
        }
    }

    /// A handle to this stub's active flag. Store `false` into it to revoke the
    /// token (subsequent `verify` calls return `Ok(false)`).
    pub fn revocation_handle(&self) -> Arc<AtomicBool> {
        self.active.clone()
    }
}

#[async_trait]
impl TokenVerifier for StubTokenVerifier {
    async fn verify(&self, token: &str) -> Result<bool, ApiError> {
        Ok(token == self.accepted && self.active.load(Ordering::SeqCst))
    }
}

/// One recorded call to the stub, with the arguments the handler passed through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    Send {
        account_id: Uuid,
        room_id: String,
        body: String,
        /// The `(format, formatted_body)` the handler passed, if any.
        formatted: Option<(String, String)>,
        /// The `reply_to` event id the handler passed, if any.
        reply_to: Option<String>,
        /// The `thread_root` event id the handler passed, if any.
        thread_root: Option<String>,
    },
    SendMedia {
        account_id: Uuid,
        room_id: String,
        attachment: MediaAttachment,
        caption: Option<String>,
        reply_to: Option<String>,
        thread_root: Option<String>,
    },
    Edit {
        account_id: Uuid,
        room_id: String,
        event_id: String,
        body: String,
        /// The `(format, formatted_body)` the handler passed, if any.
        formatted: Option<(String, String)>,
    },
    Redact {
        account_id: Uuid,
        room_id: String,
        event_id: String,
        reason: Option<String>,
    },
    React {
        account_id: Uuid,
        room_id: String,
        event_id: String,
        key: String,
    },
}

/// The outcome the stub returns for every call. `Clone` (unlike [`SendError`])
/// so one stub can answer repeated calls.
#[derive(Clone)]
pub enum Outcome {
    Ok(String),
    NotFound(String),
    Forbidden(String),
    Unavailable(String),
    Invalid(String),
    Upstream(String),
    Timeout(String),
}

impl Outcome {
    fn to_result(&self) -> Result<String, SendError> {
        match self {
            Outcome::Ok(id) => Ok(id.clone()),
            Outcome::NotFound(m) => Err(SendError::NotFound(m.clone())),
            Outcome::Forbidden(m) => Err(SendError::Forbidden(m.clone())),
            Outcome::Unavailable(m) => Err(SendError::Unavailable(m.clone())),
            Outcome::Invalid(m) => Err(SendError::Invalid(m.clone())),
            Outcome::Upstream(m) => Err(SendError::Upstream(m.clone())),
            Outcome::Timeout(m) => Err(SendError::Timeout(m.clone())),
        }
    }
}

/// An in-memory [`MessageSender`] for tests.
pub struct StubSender {
    outcome: Outcome,
    calls: Mutex<Vec<Call>>,
}

/// One staged-upload call recorded by [`StubUploads`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadCall {
    pub account_id: Uuid,
    pub kind: axon_api::MediaUploadKindDto,
    pub filename: String,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

/// The outcome [`StubUploads`] returns for every call.
#[derive(Clone)]
pub enum UploadOutcome {
    Ok,
    TooLarge { cap: u64 },
    NotFound(String),
    Forbidden(String),
    Invalid(String),
    Timeout(String),
    Internal(String),
}

/// In-memory staged-upload service for route tests.
pub struct StubUploads {
    outcome: UploadOutcome,
    calls: Mutex<Vec<UploadCall>>,
    deletes: Mutex<Vec<(Uuid, Uuid)>>,
    claims: Mutex<Vec<(Uuid, Uuid)>>,
    completes: Mutex<Vec<(Uuid, Uuid)>>,
    releases: Mutex<Vec<(Uuid, Uuid)>>,
}

impl StubUploads {
    pub fn ok() -> Self {
        Self {
            outcome: UploadOutcome::Ok,
            calls: Mutex::new(Vec::new()),
            deletes: Mutex::new(Vec::new()),
            claims: Mutex::new(Vec::new()),
            completes: Mutex::new(Vec::new()),
            releases: Mutex::new(Vec::new()),
        }
    }

    pub fn failing(outcome: UploadOutcome) -> Self {
        Self {
            outcome,
            calls: Mutex::new(Vec::new()),
            deletes: Mutex::new(Vec::new()),
            claims: Mutex::new(Vec::new()),
            completes: Mutex::new(Vec::new()),
            releases: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<UploadCall> {
        self.calls.lock().unwrap().clone()
    }

    pub fn deletes(&self) -> Vec<(Uuid, Uuid)> {
        self.deletes.lock().unwrap().clone()
    }

    pub fn claims(&self) -> Vec<(Uuid, Uuid)> {
        self.claims.lock().unwrap().clone()
    }

    pub fn completes(&self) -> Vec<(Uuid, Uuid)> {
        self.completes.lock().unwrap().clone()
    }

    pub fn releases(&self) -> Vec<(Uuid, Uuid)> {
        self.releases.lock().unwrap().clone()
    }
}

impl StubSender {
    /// A stub that returns `Ok(event_id)` for every call.
    pub fn ok(event_id: &str) -> Self {
        Self {
            outcome: Outcome::Ok(event_id.to_owned()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub that returns the given failure for every call.
    pub fn failing(outcome: Outcome) -> Self {
        Self {
            outcome,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// The calls recorded so far, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

/// One recorded call to [`StubEphemeral`], with the arguments the handler
/// passed through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EphemeralCall {
    ReadReceipt {
        account_id: Uuid,
        room_id: String,
        event_id: String,
    },
    Typing {
        account_id: Uuid,
        room_id: String,
        typing: bool,
    },
}

/// The outcome [`StubEphemeral`] returns for every call. `Clone` (unlike
/// [`SendError`]) so one stub can answer repeated calls.
#[derive(Clone)]
pub enum EphemeralOutcome {
    Ok,
    NotFound(String),
    Forbidden(String),
    Unavailable(String),
    Invalid(String),
    Upstream(String),
}

impl EphemeralOutcome {
    fn to_result(&self) -> Result<(), SendError> {
        match self {
            EphemeralOutcome::Ok => Ok(()),
            EphemeralOutcome::NotFound(m) => Err(SendError::NotFound(m.clone())),
            EphemeralOutcome::Forbidden(m) => Err(SendError::Forbidden(m.clone())),
            EphemeralOutcome::Unavailable(m) => Err(SendError::Unavailable(m.clone())),
            EphemeralOutcome::Invalid(m) => Err(SendError::Invalid(m.clone())),
            EphemeralOutcome::Upstream(m) => Err(SendError::Upstream(m.clone())),
        }
    }
}

/// An in-memory [`EphemeralSender`] for tests.
pub struct StubEphemeral {
    outcome: EphemeralOutcome,
    calls: Mutex<Vec<EphemeralCall>>,
}

impl StubEphemeral {
    /// A stub that returns `Ok(())` for every call.
    pub fn ok() -> Self {
        Self {
            outcome: EphemeralOutcome::Ok,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub that returns the given failure for every call.
    pub fn failing(outcome: EphemeralOutcome) -> Self {
        Self {
            outcome,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// The calls recorded so far, in order.
    pub fn calls(&self) -> Vec<EphemeralCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl EphemeralSender for StubEphemeral {
    async fn send_read_receipt(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
    ) -> Result<(), SendError> {
        self.calls.lock().unwrap().push(EphemeralCall::ReadReceipt {
            account_id,
            room_id: room_id.to_owned(),
            event_id: event_id.to_owned(),
        });
        self.outcome.to_result()
    }

    async fn send_typing_notice(
        &self,
        account_id: Uuid,
        room_id: &str,
        typing: bool,
    ) -> Result<(), SendError> {
        self.calls.lock().unwrap().push(EphemeralCall::Typing {
            account_id,
            room_id: room_id.to_owned(),
            typing,
        });
        self.outcome.to_result()
    }
}

/// One recorded call to [`StubMembership`], with the arguments the handler
/// passed through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MembershipCall {
    Leave {
        account_id: Uuid,
        room_id: String,
    },
    Forget {
        account_id: Uuid,
        room_id: String,
    },
    Invite {
        account_id: Uuid,
        room_id: String,
        user_id: String,
    },
    Kick {
        account_id: Uuid,
        room_id: String,
        user_id: String,
        reason: Option<String>,
    },
    Ban {
        account_id: Uuid,
        room_id: String,
        user_id: String,
        reason: Option<String>,
    },
    Unban {
        account_id: Uuid,
        room_id: String,
        user_id: String,
        reason: Option<String>,
    },
}

/// The outcome [`StubMembership`] returns for every call. `Clone` (unlike
/// [`SendError`]) so one stub can answer repeated calls.
#[derive(Clone)]
pub enum MembershipOutcome {
    Ok,
    NotFound(String),
    Forbidden(String),
    Unavailable(String),
    Invalid(String),
    Upstream(String),
}

impl MembershipOutcome {
    fn to_result(&self) -> Result<(), SendError> {
        match self {
            MembershipOutcome::Ok => Ok(()),
            MembershipOutcome::NotFound(m) => Err(SendError::NotFound(m.clone())),
            MembershipOutcome::Forbidden(m) => Err(SendError::Forbidden(m.clone())),
            MembershipOutcome::Unavailable(m) => Err(SendError::Unavailable(m.clone())),
            MembershipOutcome::Invalid(m) => Err(SendError::Invalid(m.clone())),
            MembershipOutcome::Upstream(m) => Err(SendError::Upstream(m.clone())),
        }
    }
}

/// An in-memory [`MembershipSender`] for tests.
pub struct StubMembership {
    outcome: MembershipOutcome,
    leave_outcome: LeaveOutcome,
    calls: Mutex<Vec<MembershipCall>>,
}

impl StubMembership {
    /// A stub that returns `Ok(())` for every call, reporting `leave` as a
    /// confirmed [`LeaveOutcome::Left`].
    pub fn ok() -> Self {
        Self {
            outcome: MembershipOutcome::Ok,
            leave_outcome: LeaveOutcome::Left,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub whose `leave` succeeds without establishing that the membership
    /// is gone — what the gateway reports for a homeserver `M_FORBIDDEN` on
    /// the no-local-`Room` fallback (ADR 0091).
    pub fn ok_unconfirmed() -> Self {
        Self {
            leave_outcome: LeaveOutcome::Unconfirmed,
            ..Self::ok()
        }
    }

    /// A stub whose `leave` reports that the homeserver denies knowing the
    /// room — what the gateway reports for a `404` carrying
    /// `M_NOT_FOUND`/`M_UNKNOWN` (ADR 0094).
    pub fn ok_room_gone() -> Self {
        Self {
            leave_outcome: LeaveOutcome::Gone,
            ..Self::ok()
        }
    }

    /// A stub that returns the given failure for every call.
    pub fn failing(outcome: MembershipOutcome) -> Self {
        Self {
            outcome,
            leave_outcome: LeaveOutcome::Left,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// The calls recorded so far, in order.
    pub fn calls(&self) -> Vec<MembershipCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl MembershipSender for StubMembership {
    async fn leave(&self, account_id: Uuid, room_id: &str) -> Result<LeaveOutcome, SendError> {
        self.calls.lock().unwrap().push(MembershipCall::Leave {
            account_id,
            room_id: room_id.to_owned(),
        });
        self.outcome.to_result().map(|()| self.leave_outcome)
    }

    async fn forget(&self, account_id: Uuid, room_id: &str) -> Result<(), SendError> {
        self.calls.lock().unwrap().push(MembershipCall::Forget {
            account_id,
            room_id: room_id.to_owned(),
        });
        self.outcome.to_result()
    }

    async fn invite(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
    ) -> Result<(), SendError> {
        self.calls.lock().unwrap().push(MembershipCall::Invite {
            account_id,
            room_id: room_id.to_owned(),
            user_id: user_id.to_owned(),
        });
        self.outcome.to_result()
    }

    async fn kick(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), SendError> {
        self.calls.lock().unwrap().push(MembershipCall::Kick {
            account_id,
            room_id: room_id.to_owned(),
            user_id: user_id.to_owned(),
            reason: reason.map(ToOwned::to_owned),
        });
        self.outcome.to_result()
    }

    async fn ban(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), SendError> {
        self.calls.lock().unwrap().push(MembershipCall::Ban {
            account_id,
            room_id: room_id.to_owned(),
            user_id: user_id.to_owned(),
            reason: reason.map(ToOwned::to_owned),
        });
        self.outcome.to_result()
    }

    async fn unban(
        &self,
        account_id: Uuid,
        room_id: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<(), SendError> {
        self.calls.lock().unwrap().push(MembershipCall::Unban {
            account_id,
            room_id: room_id.to_owned(),
            user_id: user_id.to_owned(),
            reason: reason.map(ToOwned::to_owned),
        });
        self.outcome.to_result()
    }
}

/// One recorded call to [`StubRoomEntry`], with the arguments the handler
/// passed through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoomEntryCall {
    Join {
        account_id: Uuid,
        room_id_or_alias: String,
        server_names: Vec<String>,
    },
    Knock {
        account_id: Uuid,
        room_id_or_alias: String,
        reason: Option<String>,
        server_names: Vec<String>,
    },
    CreateRoom {
        account_id: Uuid,
        request: CreateRoomRequest,
    },
    CreateDm {
        account_id: Uuid,
        user_id: String,
    },
}

/// The outcome [`StubRoomEntry`] returns for every call. `Clone` (unlike
/// [`SendError`]) so one stub can answer repeated calls. `Ok` carries the
/// room id to return.
#[derive(Clone)]
pub enum RoomEntryOutcome {
    Ok(String),
    NotFound(String),
    Forbidden(String),
    Unavailable(String),
    Invalid(String),
    Upstream(String),
}

impl RoomEntryOutcome {
    fn to_result(&self) -> Result<String, SendError> {
        match self {
            RoomEntryOutcome::Ok(room_id) => Ok(room_id.clone()),
            RoomEntryOutcome::NotFound(m) => Err(SendError::NotFound(m.clone())),
            RoomEntryOutcome::Forbidden(m) => Err(SendError::Forbidden(m.clone())),
            RoomEntryOutcome::Unavailable(m) => Err(SendError::Unavailable(m.clone())),
            RoomEntryOutcome::Invalid(m) => Err(SendError::Invalid(m.clone())),
            RoomEntryOutcome::Upstream(m) => Err(SendError::Upstream(m.clone())),
        }
    }
}

/// An in-memory [`RoomEntrySender`] for tests.
pub struct StubRoomEntry {
    outcome: RoomEntryOutcome,
    calls: Mutex<Vec<RoomEntryCall>>,
}

impl StubRoomEntry {
    /// A stub that returns a fixed room id for every call.
    pub fn ok() -> Self {
        Self {
            outcome: RoomEntryOutcome::Ok("!created:localhost".to_owned()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub that returns the given failure for every call.
    pub fn failing(outcome: RoomEntryOutcome) -> Self {
        Self {
            outcome,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// The calls recorded so far, in order.
    pub fn calls(&self) -> Vec<RoomEntryCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl RoomEntrySender for StubRoomEntry {
    async fn join(
        &self,
        account_id: Uuid,
        room_id_or_alias: &str,
        server_names: &[String],
    ) -> Result<String, SendError> {
        self.calls.lock().unwrap().push(RoomEntryCall::Join {
            account_id,
            room_id_or_alias: room_id_or_alias.to_owned(),
            server_names: server_names.to_vec(),
        });
        self.outcome.to_result()
    }

    async fn knock(
        &self,
        account_id: Uuid,
        room_id_or_alias: &str,
        reason: Option<&str>,
        server_names: &[String],
    ) -> Result<String, SendError> {
        self.calls.lock().unwrap().push(RoomEntryCall::Knock {
            account_id,
            room_id_or_alias: room_id_or_alias.to_owned(),
            reason: reason.map(ToOwned::to_owned),
            server_names: server_names.to_vec(),
        });
        self.outcome.to_result()
    }

    async fn create_room(
        &self,
        account_id: Uuid,
        request: CreateRoomRequest,
    ) -> Result<String, SendError> {
        self.calls.lock().unwrap().push(RoomEntryCall::CreateRoom {
            account_id,
            request,
        });
        self.outcome.to_result()
    }

    async fn create_dm(&self, account_id: Uuid, user_id: &str) -> Result<String, SendError> {
        self.calls.lock().unwrap().push(RoomEntryCall::CreateDm {
            account_id,
            user_id: user_id.to_owned(),
        });
        self.outcome.to_result()
    }
}

/// One recorded call to [`StubRoomSettings`], with the arguments the handler
/// passed through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoomSettingsCall {
    SetName {
        account_id: Uuid,
        room_id: String,
        name: String,
    },
    SetTopic {
        account_id: Uuid,
        room_id: String,
        topic: String,
    },
    SetAvatar {
        account_id: Uuid,
        room_id: String,
        attachment: MediaAttachment,
    },
    RemoveAvatar {
        account_id: Uuid,
        room_id: String,
    },
    SetTag {
        account_id: Uuid,
        room_id: String,
        tag: String,
        order: Option<OrderedFloat>,
    },
    RemoveTag {
        account_id: Uuid,
        room_id: String,
        tag: String,
    },
}

/// Wraps `Option<f64>` so [`RoomSettingsCall`] can derive `Eq` (`f64` has no
/// total order); test tags never carry `NaN`/`inf`, so bit-equality via
/// `to_bits` is a safe stand-in for the deliberately absent `PartialOrd`/`Ord`.
#[derive(Clone, Copy, Debug)]
pub struct OrderedFloat(pub f64);

impl PartialEq for OrderedFloat {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}
impl Eq for OrderedFloat {}

/// The outcome [`StubRoomSettings`] returns for every call. `Clone` (unlike
/// [`SendError`]) so one stub can answer repeated calls.
#[derive(Clone)]
pub enum RoomSettingsOutcome {
    Ok,
    NotFound(String),
    Forbidden(String),
    Unavailable(String),
    Invalid(String),
    Upstream(String),
}

impl RoomSettingsOutcome {
    fn to_result(&self) -> Result<(), SendError> {
        match self {
            RoomSettingsOutcome::Ok => Ok(()),
            RoomSettingsOutcome::NotFound(m) => Err(SendError::NotFound(m.clone())),
            RoomSettingsOutcome::Forbidden(m) => Err(SendError::Forbidden(m.clone())),
            RoomSettingsOutcome::Unavailable(m) => Err(SendError::Unavailable(m.clone())),
            RoomSettingsOutcome::Invalid(m) => Err(SendError::Invalid(m.clone())),
            RoomSettingsOutcome::Upstream(m) => Err(SendError::Upstream(m.clone())),
        }
    }
}

/// An in-memory [`RoomSettingsSender`] for tests.
pub struct StubRoomSettings {
    outcome: RoomSettingsOutcome,
    calls: Mutex<Vec<RoomSettingsCall>>,
}

impl StubRoomSettings {
    /// A stub that returns `Ok(())` for every call.
    pub fn ok() -> Self {
        Self {
            outcome: RoomSettingsOutcome::Ok,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub that returns the given failure for every call.
    pub fn failing(outcome: RoomSettingsOutcome) -> Self {
        Self {
            outcome,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// The calls recorded so far, in order.
    pub fn calls(&self) -> Vec<RoomSettingsCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl RoomSettingsSender for StubRoomSettings {
    async fn set_name(&self, account_id: Uuid, room_id: &str, name: &str) -> Result<(), SendError> {
        self.calls.lock().unwrap().push(RoomSettingsCall::SetName {
            account_id,
            room_id: room_id.to_owned(),
            name: name.to_owned(),
        });
        self.outcome.to_result()
    }

    async fn set_topic(
        &self,
        account_id: Uuid,
        room_id: &str,
        topic: &str,
    ) -> Result<(), SendError> {
        self.calls.lock().unwrap().push(RoomSettingsCall::SetTopic {
            account_id,
            room_id: room_id.to_owned(),
            topic: topic.to_owned(),
        });
        self.outcome.to_result()
    }

    async fn set_avatar(
        &self,
        account_id: Uuid,
        room_id: &str,
        attachment: MediaAttachment,
    ) -> Result<(), SendError> {
        self.calls
            .lock()
            .unwrap()
            .push(RoomSettingsCall::SetAvatar {
                account_id,
                room_id: room_id.to_owned(),
                attachment,
            });
        self.outcome.to_result()
    }

    async fn remove_avatar(&self, account_id: Uuid, room_id: &str) -> Result<(), SendError> {
        self.calls
            .lock()
            .unwrap()
            .push(RoomSettingsCall::RemoveAvatar {
                account_id,
                room_id: room_id.to_owned(),
            });
        self.outcome.to_result()
    }

    async fn set_tag(
        &self,
        account_id: Uuid,
        room_id: &str,
        tag: &str,
        order: Option<f64>,
    ) -> Result<(), SendError> {
        self.calls.lock().unwrap().push(RoomSettingsCall::SetTag {
            account_id,
            room_id: room_id.to_owned(),
            tag: tag.to_owned(),
            order: order.map(OrderedFloat),
        });
        self.outcome.to_result()
    }

    async fn remove_tag(
        &self,
        account_id: Uuid,
        room_id: &str,
        tag: &str,
    ) -> Result<(), SendError> {
        self.calls
            .lock()
            .unwrap()
            .push(RoomSettingsCall::RemoveTag {
                account_id,
                room_id: room_id.to_owned(),
                tag: tag.to_owned(),
            });
        self.outcome.to_result()
    }
}

/// One recorded call to [`StubPowerLevels`], with the arguments the handler
/// passed through.
#[derive(Clone, Debug, PartialEq)]
pub enum PowerLevelsCall {
    SetPowerLevels {
        account_id: Uuid,
        room_id: String,
        changes: PowerLevelChanges,
    },
    GetPowerLevels {
        account_id: Uuid,
        room_id: String,
    },
}

/// The outcome [`StubPowerLevels`] returns for every `set_power_levels` call
/// (`Clone`, unlike [`SendError`], so one stub can answer repeated calls) and
/// the fixed value it returns for every `power_levels` read.
#[derive(Clone)]
pub enum PowerLevelsOutcome {
    Ok,
    NotFound(String),
    Forbidden(String),
    Unavailable(String),
    Invalid(String),
    Upstream(String),
}

impl PowerLevelsOutcome {
    fn to_result(&self) -> Result<(), SendError> {
        match self {
            PowerLevelsOutcome::Ok => Ok(()),
            PowerLevelsOutcome::NotFound(m) => Err(SendError::NotFound(m.clone())),
            PowerLevelsOutcome::Forbidden(m) => Err(SendError::Forbidden(m.clone())),
            PowerLevelsOutcome::Unavailable(m) => Err(SendError::Unavailable(m.clone())),
            PowerLevelsOutcome::Invalid(m) => Err(SendError::Invalid(m.clone())),
            PowerLevelsOutcome::Upstream(m) => Err(SendError::Upstream(m.clone())),
        }
    }
}

/// An in-memory [`PowerLevelsSender`] for tests. The self-demotion guardrail
/// itself lives in `axon-sync`'s `SdkGateway` (covered by its own unit tests,
/// `gateway::tests::check_self_demotion_guardrail_*`) — this stub only
/// exercises routing, request decoding, and error-mapping at the handler
/// layer, same division of labor as [`StubRoomSettings`].
pub struct StubPowerLevels {
    outcome: PowerLevelsOutcome,
    read: ResolvedPowerLevels,
    calls: Mutex<Vec<PowerLevelsCall>>,
}

impl StubPowerLevels {
    /// A stub that returns `Ok(())` for writes and a zeroed read for reads.
    pub fn ok() -> Self {
        Self {
            outcome: PowerLevelsOutcome::Ok,
            read: ResolvedPowerLevels::default(),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub whose write fails with the given outcome; reads still succeed.
    pub fn failing(outcome: PowerLevelsOutcome) -> Self {
        Self {
            outcome,
            read: ResolvedPowerLevels::default(),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub whose read returns `resolved`.
    pub fn with_read(resolved: ResolvedPowerLevels) -> Self {
        Self {
            outcome: PowerLevelsOutcome::Ok,
            read: resolved,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// The calls recorded so far, in order.
    pub fn calls(&self) -> Vec<PowerLevelsCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl PowerLevelsSender for StubPowerLevels {
    async fn set_power_levels(
        &self,
        account_id: Uuid,
        room_id: &str,
        changes: PowerLevelChanges,
    ) -> Result<(), SendError> {
        self.calls
            .lock()
            .unwrap()
            .push(PowerLevelsCall::SetPowerLevels {
                account_id,
                room_id: room_id.to_owned(),
                changes,
            });
        self.outcome.to_result()
    }

    async fn power_levels(
        &self,
        account_id: Uuid,
        room_id: &str,
    ) -> Result<ResolvedPowerLevels, SendError> {
        self.calls
            .lock()
            .unwrap()
            .push(PowerLevelsCall::GetPowerLevels {
                account_id,
                room_id: room_id.to_owned(),
            });
        Ok(self.read.clone())
    }
}

/// One recorded call to [`StubAccountActions`], with the arguments the
/// handler passed through.
#[derive(Clone, Debug, PartialEq)]
pub enum AccountActionsCall {
    SetDisplayName {
        account_id: Uuid,
        display_name: String,
    },
    SetAvatar {
        account_id: Uuid,
        attachment: MediaAttachment,
    },
    RemoveAvatar {
        account_id: Uuid,
    },
    UserProfile {
        account_id: Uuid,
        user_id: String,
    },
    IgnoreUser {
        account_id: Uuid,
        user_id: String,
    },
    UnignoreUser {
        account_id: Uuid,
        user_id: String,
    },
    PublicRooms {
        account_id: Uuid,
        query: PublicRoomsQuery,
    },
}

/// The outcome [`StubAccountActions`] returns for every mutating call
/// (`Clone`, unlike [`SendError`], so one stub can answer repeated calls);
/// the fixed values it returns for the two reads live separately.
#[derive(Clone)]
pub enum AccountActionsOutcome {
    Ok,
    NotFound(String),
    Forbidden(String),
    Unavailable(String),
    Invalid(String),
    Upstream(String),
}

impl AccountActionsOutcome {
    fn to_result(&self) -> Result<(), SendError> {
        match self {
            AccountActionsOutcome::Ok => Ok(()),
            AccountActionsOutcome::NotFound(m) => Err(SendError::NotFound(m.clone())),
            AccountActionsOutcome::Forbidden(m) => Err(SendError::Forbidden(m.clone())),
            AccountActionsOutcome::Unavailable(m) => Err(SendError::Unavailable(m.clone())),
            AccountActionsOutcome::Invalid(m) => Err(SendError::Invalid(m.clone())),
            AccountActionsOutcome::Upstream(m) => Err(SendError::Upstream(m.clone())),
        }
    }
    fn to_read_result<T: Clone>(&self, ok: &T) -> Result<T, SendError> {
        self.to_result().map(|()| ok.clone())
    }
}

/// An in-memory [`AccountActionsSender`] for tests, mirroring
/// [`StubPowerLevels`]'s division of labor: only routing, request decoding,
/// and error-mapping at the handler layer, without a real homeserver.
pub struct StubAccountActions {
    outcome: AccountActionsOutcome,
    profile: MatrixProfile,
    public_rooms: PublicRoomsPage,
    calls: Mutex<Vec<AccountActionsCall>>,
}

impl StubAccountActions {
    /// A stub that returns `Ok(())` for mutations and empty/default values
    /// for reads.
    pub fn ok() -> Self {
        Self {
            outcome: AccountActionsOutcome::Ok,
            profile: MatrixProfile::default(),
            public_rooms: PublicRoomsPage::default(),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub whose mutations fail with the given outcome; reads still succeed.
    pub fn failing(outcome: AccountActionsOutcome) -> Self {
        Self {
            outcome,
            profile: MatrixProfile::default(),
            public_rooms: PublicRoomsPage::default(),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub whose `user_profile` read returns `profile`.
    pub fn with_profile(profile: MatrixProfile) -> Self {
        Self {
            outcome: AccountActionsOutcome::Ok,
            profile,
            public_rooms: PublicRoomsPage::default(),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub whose `public_rooms` read returns `page`.
    pub fn with_public_rooms(page: PublicRoomsPage) -> Self {
        Self {
            outcome: AccountActionsOutcome::Ok,
            profile: MatrixProfile::default(),
            public_rooms: page,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// The calls recorded so far, in order.
    pub fn calls(&self) -> Vec<AccountActionsCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl AccountActionsSender for StubAccountActions {
    async fn set_display_name(
        &self,
        account_id: Uuid,
        display_name: &str,
    ) -> Result<(), SendError> {
        self.calls
            .lock()
            .unwrap()
            .push(AccountActionsCall::SetDisplayName {
                account_id,
                display_name: display_name.to_owned(),
            });
        self.outcome.to_result()
    }

    async fn set_avatar(
        &self,
        account_id: Uuid,
        attachment: MediaAttachment,
    ) -> Result<(), SendError> {
        self.calls
            .lock()
            .unwrap()
            .push(AccountActionsCall::SetAvatar {
                account_id,
                attachment,
            });
        self.outcome.to_result()
    }

    async fn remove_avatar(&self, account_id: Uuid) -> Result<(), SendError> {
        self.calls
            .lock()
            .unwrap()
            .push(AccountActionsCall::RemoveAvatar { account_id });
        self.outcome.to_result()
    }

    async fn user_profile(
        &self,
        account_id: Uuid,
        user_id: &str,
    ) -> Result<MatrixProfile, SendError> {
        self.calls
            .lock()
            .unwrap()
            .push(AccountActionsCall::UserProfile {
                account_id,
                user_id: user_id.to_owned(),
            });
        self.outcome.to_read_result(&self.profile)
    }

    async fn ignore_user(&self, account_id: Uuid, user_id: &str) -> Result<(), SendError> {
        self.calls
            .lock()
            .unwrap()
            .push(AccountActionsCall::IgnoreUser {
                account_id,
                user_id: user_id.to_owned(),
            });
        self.outcome.to_result()
    }

    async fn unignore_user(&self, account_id: Uuid, user_id: &str) -> Result<(), SendError> {
        self.calls
            .lock()
            .unwrap()
            .push(AccountActionsCall::UnignoreUser {
                account_id,
                user_id: user_id.to_owned(),
            });
        self.outcome.to_result()
    }

    async fn public_rooms(
        &self,
        account_id: Uuid,
        query: PublicRoomsQuery,
    ) -> Result<PublicRoomsPage, SendError> {
        self.calls
            .lock()
            .unwrap()
            .push(AccountActionsCall::PublicRooms { account_id, query });
        self.outcome.to_read_result(&self.public_rooms)
    }
}

/// The outcome the [`StubLifecycle`] returns for every `login` call. `Clone` so
/// one stub can answer repeated calls; mirrors [`LoginError`]'s variants.
#[derive(Clone)]
pub enum LoginOutcome {
    Ok(Uuid),
    InvalidRequest(String),
    AuthFailed(String),
    Conflict(String),
    Upstream(String),
    Internal,
}

impl LoginOutcome {
    fn to_result(&self) -> Result<Uuid, LoginError> {
        match self {
            LoginOutcome::Ok(id) => Ok(*id),
            LoginOutcome::InvalidRequest(m) => Err(LoginError::InvalidRequest(m.clone())),
            LoginOutcome::AuthFailed(m) => Err(LoginError::AuthFailed(m.clone())),
            LoginOutcome::Conflict(m) => Err(LoginError::Conflict(m.clone())),
            LoginOutcome::Upstream(m) => Err(LoginError::Upstream(m.clone())),
            LoginOutcome::Internal => Err(LoginError::Internal),
        }
    }
}

/// One recorded `login` call, with the arguments the handler passed through.
/// `homeserver_url` is `None` when the request omitted it (server-side
/// discovery — the stub records exactly what the handler forwarded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginCall {
    pub homeserver_url: Option<String>,
    pub username: String,
    pub password: String,
}

/// One recorded `import_token` call, with the arguments the handler passed
/// through. Unlike [`LoginCall`], `homeserver_url` is never optional — there is
/// no MXID-based discovery for an imported token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportTokenCall {
    pub homeserver_url: String,
    pub username: String,
    pub access_token: String,
    pub device_id: String,
}

/// The outcome the [`StubLifecycle`] returns for every `logout` call. `Clone` so
/// one stub can answer repeated calls; mirrors [`LogoutError`]'s variants.
#[derive(Clone)]
pub enum LogoutOutcome {
    Ok,
    NotFound(String),
    Conflict(String),
    Internal,
}

impl LogoutOutcome {
    fn to_result(&self) -> Result<(), LogoutError> {
        match self {
            LogoutOutcome::Ok => Ok(()),
            LogoutOutcome::NotFound(m) => Err(LogoutError::NotFound(m.clone())),
            LogoutOutcome::Conflict(m) => Err(LogoutError::Conflict(m.clone())),
            LogoutOutcome::Internal => Err(LogoutError::Internal),
        }
    }
}

/// The outcome the [`StubLifecycle`] returns for every `delete` call. `Clone` so
/// one stub can answer repeated calls; mirrors [`DeleteError`]'s variants.
#[derive(Clone)]
pub enum DeleteOutcome {
    Ok,
    NotFound(String),
    Conflict(String),
    Internal,
}

impl DeleteOutcome {
    fn to_result(&self) -> Result<(), DeleteError> {
        match self {
            DeleteOutcome::Ok => Ok(()),
            DeleteOutcome::NotFound(m) => Err(DeleteError::NotFound(m.clone())),
            DeleteOutcome::Conflict(m) => Err(DeleteError::Conflict(m.clone())),
            DeleteOutcome::Internal => Err(DeleteError::Internal),
        }
    }
}

/// The outcome the [`StubLifecycle`] returns for every `recover` call. `Clone` so
/// one stub can answer repeated calls; mirrors [`RecoverError`]'s variants.
#[derive(Clone)]
pub enum RecoverOutcome {
    Ok,
    NotFound(String),
    Conflict(String),
    BadRequest(String),
    Internal,
}

impl RecoverOutcome {
    fn to_result(&self) -> Result<(), RecoverError> {
        match self {
            RecoverOutcome::Ok => Ok(()),
            RecoverOutcome::NotFound(m) => Err(RecoverError::NotFound(m.clone())),
            RecoverOutcome::Conflict(m) => Err(RecoverError::Conflict(m.clone())),
            RecoverOutcome::BadRequest(m) => Err(RecoverError::BadRequest(m.clone())),
            RecoverOutcome::Internal => Err(RecoverError::Internal),
        }
    }
}

/// The outcome the [`StubLifecycle`] returns for every `redecrypt_utds` call.
#[derive(Clone)]
pub enum RedecryptOutcome {
    Ok(RedecryptUtdsStats),
    NotFound(String),
    Conflict(String),
    Internal,
}

impl RedecryptOutcome {
    fn to_result(&self) -> Result<RedecryptUtdsStats, RedecryptUtdsError> {
        match self {
            RedecryptOutcome::Ok(stats) => Ok(*stats),
            RedecryptOutcome::NotFound(m) => Err(RedecryptUtdsError::NotFound(m.clone())),
            RedecryptOutcome::Conflict(m) => Err(RedecryptUtdsError::Conflict(m.clone())),
            RedecryptOutcome::Internal => Err(RedecryptUtdsError::Internal),
        }
    }
}

/// An in-memory [`AccountLifecycle`] for tests: records each
/// `login`/`logout`/`delete`/`recover` call and returns a preset outcome, so the
/// lifecycle routes can be exercised (auth gate, request decoding, error →
/// status mapping) without a real homeserver.
pub struct StubLifecycle {
    login_outcome: LoginOutcome,
    import_token_outcome: LoginOutcome,
    logout_outcome: LogoutOutcome,
    delete_outcome: DeleteOutcome,
    recover_outcome: RecoverOutcome,
    redecrypt_outcome: RedecryptOutcome,
    login_calls: Mutex<Vec<LoginCall>>,
    import_token_calls: Mutex<Vec<ImportTokenCall>>,
    logout_calls: Mutex<Vec<Uuid>>,
    delete_calls: Mutex<Vec<Uuid>>,
    recover_calls: Mutex<Vec<(Uuid, String)>>,
    redecrypt_calls: Mutex<Vec<Uuid>>,
}

impl StubLifecycle {
    /// A stub that succeeds for every login, import, logout, and delete.
    pub fn ok(account_id: Uuid) -> Self {
        Self {
            login_outcome: LoginOutcome::Ok(account_id),
            import_token_outcome: LoginOutcome::Ok(account_id),
            logout_outcome: LogoutOutcome::Ok,
            delete_outcome: DeleteOutcome::Ok,
            login_calls: Mutex::new(Vec::new()),
            import_token_calls: Mutex::new(Vec::new()),
            logout_calls: Mutex::new(Vec::new()),
            delete_calls: Mutex::new(Vec::new()),
            recover_outcome: RecoverOutcome::Ok,
            recover_calls: Mutex::new(Vec::new()),
            redecrypt_outcome: RedecryptOutcome::Ok(RedecryptUtdsStats::default()),
            redecrypt_calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub whose login returns the given failure (logout/delete succeed).
    pub fn failing(outcome: LoginOutcome) -> Self {
        Self {
            login_outcome: outcome,
            import_token_outcome: LoginOutcome::Ok(Uuid::nil()),
            logout_outcome: LogoutOutcome::Ok,
            delete_outcome: DeleteOutcome::Ok,
            login_calls: Mutex::new(Vec::new()),
            import_token_calls: Mutex::new(Vec::new()),
            logout_calls: Mutex::new(Vec::new()),
            delete_calls: Mutex::new(Vec::new()),
            recover_outcome: RecoverOutcome::Ok,
            recover_calls: Mutex::new(Vec::new()),
            redecrypt_outcome: RedecryptOutcome::Ok(RedecryptUtdsStats::default()),
            redecrypt_calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub whose `import_token` returns the given failure (login succeeds
    /// with a nil id).
    pub fn import_token_failing(outcome: LoginOutcome) -> Self {
        Self {
            login_outcome: LoginOutcome::Ok(Uuid::nil()),
            import_token_outcome: outcome,
            logout_outcome: LogoutOutcome::Ok,
            delete_outcome: DeleteOutcome::Ok,
            login_calls: Mutex::new(Vec::new()),
            import_token_calls: Mutex::new(Vec::new()),
            logout_calls: Mutex::new(Vec::new()),
            delete_calls: Mutex::new(Vec::new()),
            recover_outcome: RecoverOutcome::Ok,
            recover_calls: Mutex::new(Vec::new()),
            redecrypt_outcome: RedecryptOutcome::Ok(RedecryptUtdsStats::default()),
            redecrypt_calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub whose logout returns the given failure (login returns a nil id).
    pub fn logout_failing(outcome: LogoutOutcome) -> Self {
        Self {
            login_outcome: LoginOutcome::Ok(Uuid::nil()),
            import_token_outcome: LoginOutcome::Ok(Uuid::nil()),
            logout_outcome: outcome,
            delete_outcome: DeleteOutcome::Ok,
            login_calls: Mutex::new(Vec::new()),
            import_token_calls: Mutex::new(Vec::new()),
            logout_calls: Mutex::new(Vec::new()),
            delete_calls: Mutex::new(Vec::new()),
            recover_outcome: RecoverOutcome::Ok,
            recover_calls: Mutex::new(Vec::new()),
            redecrypt_outcome: RedecryptOutcome::Ok(RedecryptUtdsStats::default()),
            redecrypt_calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub whose delete returns the given failure (login returns a nil id).
    pub fn delete_failing(outcome: DeleteOutcome) -> Self {
        Self {
            login_outcome: LoginOutcome::Ok(Uuid::nil()),
            import_token_outcome: LoginOutcome::Ok(Uuid::nil()),
            logout_outcome: LogoutOutcome::Ok,
            delete_outcome: outcome,
            login_calls: Mutex::new(Vec::new()),
            import_token_calls: Mutex::new(Vec::new()),
            logout_calls: Mutex::new(Vec::new()),
            delete_calls: Mutex::new(Vec::new()),
            recover_outcome: RecoverOutcome::Ok,
            recover_calls: Mutex::new(Vec::new()),
            redecrypt_outcome: RedecryptOutcome::Ok(RedecryptUtdsStats::default()),
            redecrypt_calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub whose recover returns the given failure (login returns a nil id).
    pub fn recover_failing(outcome: RecoverOutcome) -> Self {
        Self {
            login_outcome: LoginOutcome::Ok(Uuid::nil()),
            import_token_outcome: LoginOutcome::Ok(Uuid::nil()),
            logout_outcome: LogoutOutcome::Ok,
            delete_outcome: DeleteOutcome::Ok,
            recover_outcome: outcome,
            login_calls: Mutex::new(Vec::new()),
            import_token_calls: Mutex::new(Vec::new()),
            logout_calls: Mutex::new(Vec::new()),
            delete_calls: Mutex::new(Vec::new()),
            recover_calls: Mutex::new(Vec::new()),
            redecrypt_outcome: RedecryptOutcome::Ok(RedecryptUtdsStats::default()),
            redecrypt_calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub whose manual UTD retry returns the given outcome.
    pub fn redecrypt_failing(outcome: RedecryptOutcome) -> Self {
        Self {
            login_outcome: LoginOutcome::Ok(Uuid::nil()),
            import_token_outcome: LoginOutcome::Ok(Uuid::nil()),
            logout_outcome: LogoutOutcome::Ok,
            delete_outcome: DeleteOutcome::Ok,
            recover_outcome: RecoverOutcome::Ok,
            redecrypt_outcome: outcome,
            login_calls: Mutex::new(Vec::new()),
            import_token_calls: Mutex::new(Vec::new()),
            logout_calls: Mutex::new(Vec::new()),
            delete_calls: Mutex::new(Vec::new()),
            recover_calls: Mutex::new(Vec::new()),
            redecrypt_calls: Mutex::new(Vec::new()),
        }
    }

    /// The login calls recorded so far, in order.
    pub fn calls(&self) -> Vec<LoginCall> {
        self.login_calls.lock().unwrap().clone()
    }

    /// The `import_token` calls recorded so far, in order.
    pub fn import_token_calls(&self) -> Vec<ImportTokenCall> {
        self.import_token_calls.lock().unwrap().clone()
    }

    /// The account ids passed to logout, in order.
    pub fn logout_calls(&self) -> Vec<Uuid> {
        self.logout_calls.lock().unwrap().clone()
    }

    /// The account ids passed to delete, in order.
    pub fn delete_calls(&self) -> Vec<Uuid> {
        self.delete_calls.lock().unwrap().clone()
    }

    /// The `(account_id, recovery_key)` pairs passed to recover, in order.
    pub fn recover_calls(&self) -> Vec<(Uuid, String)> {
        self.recover_calls.lock().unwrap().clone()
    }

    /// The account ids passed to manual UTD retry, in order.
    pub fn redecrypt_calls(&self) -> Vec<Uuid> {
        self.redecrypt_calls.lock().unwrap().clone()
    }
}

/// One recorded call to the [`StubVerification`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyCall {
    Start {
        account_id: Uuid,
        user_id: Option<String>,
        device_id: Option<String>,
    },
    List {
        account_id: Uuid,
    },
    Get {
        account_id: Uuid,
        flow_id: String,
    },
    Confirm {
        account_id: Uuid,
        flow_id: String,
    },
    Cancel {
        account_id: Uuid,
        flow_id: String,
    },
}

/// The error the [`StubVerification`] returns for every call (or `Ok`). `Clone`
/// so one stub can answer repeated calls; mirrors [`VerifyError`]'s variants.
#[derive(Clone)]
pub enum VerifyOutcome {
    Ok,
    NotFound(String),
    NotActive(String),
    Conflict(String),
    BadRequest(String),
    Upstream(String),
    Internal,
}

impl VerifyOutcome {
    /// The error to return, or `None` when the outcome is `Ok` (the method then
    /// returns its natural success value).
    fn as_error(&self) -> Option<VerifyError> {
        match self {
            VerifyOutcome::Ok => None,
            VerifyOutcome::NotFound(m) => Some(VerifyError::NotFound(m.clone())),
            VerifyOutcome::NotActive(m) => Some(VerifyError::NotActive(m.clone())),
            VerifyOutcome::Conflict(m) => Some(VerifyError::Conflict(m.clone())),
            VerifyOutcome::BadRequest(m) => Some(VerifyError::BadRequest(m.clone())),
            VerifyOutcome::Upstream(m) => Some(VerifyError::Upstream(m.clone())),
            VerifyOutcome::Internal => Some(VerifyError::Internal),
        }
    }
}

/// An in-memory [`VerificationService`] for tests: records each call and returns
/// a preset outcome, so the verify routes can be exercised (auth gate, path
/// /body decoding, error → status mapping, DTO shape) without a real client.
pub struct StubVerification {
    outcome: VerifyOutcome,
    flow_id: String,
    summary: FlowSummary,
    calls: Mutex<Vec<VerifyCall>>,
}

impl StubVerification {
    /// A stub that succeeds for every op: `start` returns `flow_id`, and
    /// `get`/`list` return a representative SAS-stage flow (emoji + decimals
    /// populated) so a test can assert the [`FlowDto`](axon_api) wire mapping.
    pub fn ok(flow_id: &str) -> Self {
        Self {
            outcome: VerifyOutcome::Ok,
            flow_id: flow_id.to_owned(),
            summary: FlowSummary {
                flow_id: flow_id.to_owned(),
                target_user_id: "@self:localhost".to_owned(),
                target_device_id: Some("TRUSTEDDEV".to_owned()),
                stage: FlowStage::KeysExchanged,
                emoji: Some(vec![
                    ("🐶".to_owned(), "Dog".to_owned()),
                    ("🐱".to_owned(), "Cat".to_owned()),
                ]),
                decimals: Some((1234, 5678, 9012)),
                cancel_reason: None,
            },
            calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub that returns the given failure for every op.
    pub fn failing(outcome: VerifyOutcome) -> Self {
        let mut s = Self::ok("$unused-flow");
        s.outcome = outcome;
        s
    }

    /// The calls recorded so far, in order.
    pub fn calls(&self) -> Vec<VerifyCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl VerificationService for StubVerification {
    async fn start(
        &self,
        account_id: Uuid,
        user_id: Option<&str>,
        device_id: Option<&str>,
    ) -> Result<String, VerifyError> {
        self.calls.lock().unwrap().push(VerifyCall::Start {
            account_id,
            user_id: user_id.map(str::to_owned),
            device_id: device_id.map(str::to_owned),
        });
        match self.outcome.as_error() {
            Some(err) => Err(err),
            None => Ok(self.flow_id.clone()),
        }
    }

    async fn list(&self, account_id: Uuid) -> Result<Vec<FlowSummary>, VerifyError> {
        self.calls
            .lock()
            .unwrap()
            .push(VerifyCall::List { account_id });
        match self.outcome.as_error() {
            Some(err) => Err(err),
            None => Ok(vec![self.summary.clone()]),
        }
    }

    async fn get(&self, account_id: Uuid, flow_id: &str) -> Result<FlowSummary, VerifyError> {
        self.calls.lock().unwrap().push(VerifyCall::Get {
            account_id,
            flow_id: flow_id.to_owned(),
        });
        match self.outcome.as_error() {
            Some(err) => Err(err),
            None => Ok(self.summary.clone()),
        }
    }

    async fn confirm(&self, account_id: Uuid, flow_id: &str) -> Result<(), VerifyError> {
        self.calls.lock().unwrap().push(VerifyCall::Confirm {
            account_id,
            flow_id: flow_id.to_owned(),
        });
        match self.outcome.as_error() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    async fn cancel(&self, account_id: Uuid, flow_id: &str) -> Result<(), VerifyError> {
        self.calls.lock().unwrap().push(VerifyCall::Cancel {
            account_id,
            flow_id: flow_id.to_owned(),
        });
        match self.outcome.as_error() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl AccountLifecycle for StubLifecycle {
    async fn login(
        &self,
        homeserver_url: Option<&str>,
        username: &str,
        password: &str,
    ) -> Result<Uuid, LoginError> {
        self.login_calls.lock().unwrap().push(LoginCall {
            homeserver_url: homeserver_url.map(str::to_owned),
            username: username.to_owned(),
            password: password.to_owned(),
        });
        self.login_outcome.to_result()
    }

    async fn import_token(
        &self,
        homeserver_url: &str,
        username: &str,
        access_token: &str,
        device_id: &str,
    ) -> Result<Uuid, LoginError> {
        self.import_token_calls
            .lock()
            .unwrap()
            .push(ImportTokenCall {
                homeserver_url: homeserver_url.to_owned(),
                username: username.to_owned(),
                access_token: access_token.to_owned(),
                device_id: device_id.to_owned(),
            });
        self.import_token_outcome.to_result()
    }

    async fn logout(&self, account_id: Uuid) -> Result<(), LogoutError> {
        self.logout_calls.lock().unwrap().push(account_id);
        self.logout_outcome.to_result()
    }

    async fn delete(&self, account_id: Uuid) -> Result<(), DeleteError> {
        self.delete_calls.lock().unwrap().push(account_id);
        self.delete_outcome.to_result()
    }

    async fn recover(&self, account_id: Uuid, recovery_key: &str) -> Result<(), RecoverError> {
        self.recover_calls
            .lock()
            .unwrap()
            .push((account_id, recovery_key.to_owned()));
        self.recover_outcome.to_result()
    }

    async fn redecrypt_utds(
        &self,
        account_id: Uuid,
    ) -> Result<RedecryptUtdsStats, RedecryptUtdsError> {
        self.redecrypt_calls.lock().unwrap().push(account_id);
        self.redecrypt_outcome.to_result()
    }
}

/// A no-op [`MediaProxy`] for tests that don't exercise the media route.
pub struct StubMediaProxy;

#[async_trait]
impl MediaProxy for StubMediaProxy {
    async fn get_media(
        &self,
        _account_id: Uuid,
        _mxc_url: &str,
        _encrypted_file: Option<serde_json::Value>,
    ) -> Result<MediaResource, MediaError> {
        Err(MediaError::NotFound("stub: no media".to_owned()))
    }

    fn etag(&self, mxc_url: &str) -> String {
        format!("stub-{mxc_url}")
    }

    async fn get_thumbnail(
        &self,
        _account_id: Uuid,
        _mxc_url: &str,
        _spec: axon_core::media::ThumbnailSpec,
    ) -> Result<MediaResource, MediaError> {
        Err(MediaError::NotFound("stub: no thumbnail".to_owned()))
    }

    fn etag_thumbnail(&self, mxc_url: &str, _spec: axon_core::media::ThumbnailSpec) -> String {
        format!("stub-thumb-{mxc_url}")
    }
}

/// Build a [`MediaResource`] backed by an open (then unlinked) temp file, the
/// way the real cache hands the handler an fd whose bytes survive eviction.
async fn media_resource_from(bytes: &[u8]) -> MediaResource {
    use tokio::io::AsyncWriteExt;
    let path = std::env::temp_dir().join(format!("axon-media-test-{}", Uuid::new_v4()));
    let mut file = tokio::fs::File::create(&path).await.expect("create temp");
    file.write_all(bytes).await.expect("write temp");
    file.sync_all().await.expect("sync temp");
    drop(file);
    let file = tokio::fs::File::open(&path).await.expect("open temp");
    let _ = std::fs::remove_file(&path); // fd stays valid after unlink
    MediaResource {
        file,
        len: bytes.len() as u64,
        etag: "test-etag".to_owned(),
    }
}

/// One call recorded by [`ConfiguredMediaProxy`].
#[derive(Clone, Debug, PartialEq)]
pub struct MediaCall {
    pub account_id: Uuid,
    pub mxc_url: String,
    pub encrypted_file: Option<serde_json::Value>,
}

/// One thumbnail call recorded by [`ConfiguredMediaProxy`].
#[derive(Clone, Debug, PartialEq)]
pub struct ThumbnailCall {
    pub account_id: Uuid,
    pub mxc_url: String,
    pub spec: axon_core::media::ThumbnailSpec,
}

/// The outcome [`ConfiguredMediaProxy`] returns for every call.
#[derive(Clone)]
pub enum MediaOutcome {
    Ok(Vec<u8>),
    Forbidden(String),
    NotConnected(String),
}

/// A configurable media proxy for route-level status and call-path tests.
pub struct ConfiguredMediaProxy {
    outcome: MediaOutcome,
    calls: Mutex<Vec<MediaCall>>,
    thumbnail_calls: Mutex<Vec<ThumbnailCall>>,
}

impl ConfiguredMediaProxy {
    pub fn ok(data: &[u8]) -> Self {
        Self {
            outcome: MediaOutcome::Ok(data.to_vec()),
            calls: Mutex::new(Vec::new()),
            thumbnail_calls: Mutex::new(Vec::new()),
        }
    }

    pub fn failing(outcome: MediaOutcome) -> Self {
        Self {
            outcome,
            calls: Mutex::new(Vec::new()),
            thumbnail_calls: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<MediaCall> {
        self.calls.lock().unwrap().clone()
    }

    pub fn thumbnail_calls(&self) -> Vec<ThumbnailCall> {
        self.thumbnail_calls.lock().unwrap().clone()
    }

    /// The configured outcome, materialized into a result — shared by
    /// `get_media` and `get_thumbnail`, which differ only in which call log
    /// they record to before resolving this.
    async fn resource_from_outcome(&self) -> Result<MediaResource, MediaError> {
        match &self.outcome {
            MediaOutcome::Ok(data) => Ok(media_resource_from(data).await),
            MediaOutcome::Forbidden(message) => Err(MediaError::Forbidden(message.clone())),
            MediaOutcome::NotConnected(message) => Err(MediaError::NotConnected(message.clone())),
        }
    }
}

#[async_trait]
impl MediaProxy for ConfiguredMediaProxy {
    async fn get_media(
        &self,
        account_id: Uuid,
        mxc_url: &str,
        encrypted_file: Option<serde_json::Value>,
    ) -> Result<MediaResource, MediaError> {
        self.calls.lock().unwrap().push(MediaCall {
            account_id,
            mxc_url: mxc_url.to_owned(),
            encrypted_file,
        });
        self.resource_from_outcome().await
    }

    fn etag(&self, mxc_url: &str) -> String {
        format!("configured-{mxc_url}")
    }

    async fn get_thumbnail(
        &self,
        account_id: Uuid,
        mxc_url: &str,
        spec: axon_core::media::ThumbnailSpec,
    ) -> Result<MediaResource, MediaError> {
        self.thumbnail_calls.lock().unwrap().push(ThumbnailCall {
            account_id,
            mxc_url: mxc_url.to_owned(),
            spec,
        });
        self.resource_from_outcome().await
    }

    fn etag_thumbnail(&self, mxc_url: &str, spec: axon_core::media::ThumbnailSpec) -> String {
        format!(
            "configured-thumb-{mxc_url}-{}x{}-{}",
            spec.width, spec.height, spec.method
        )
    }
}

#[async_trait]
impl StagedUploadService for StubUploads {
    async fn stage_upload(
        &self,
        request: StageUploadRequest,
        mut body: UploadStream,
    ) -> Result<StagedUpload, StageUploadError> {
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            bytes.extend_from_slice(
                &chunk.map_err(|err| StageUploadError::Invalid(err.to_string()))?,
            );
        }
        self.calls.lock().unwrap().push(UploadCall {
            account_id: request.account_id,
            kind: request.kind,
            filename: request.filename.clone(),
            content_type: request.content_type.clone(),
            bytes,
        });
        match &self.outcome {
            UploadOutcome::Ok => Ok(StagedUpload {
                upload_id: Uuid::new_v4(),
                kind: request.kind,
                filename: request.filename,
                content_type: request.content_type,
                size_bytes: self
                    .calls
                    .lock()
                    .unwrap()
                    .last()
                    .map(|call| call.bytes.len() as u64)
                    .unwrap_or(0),
                expires_at: "2026-07-11T00:00:00Z".to_owned(),
            }),
            UploadOutcome::TooLarge { cap } => Err(StageUploadError::TooLarge { cap: *cap }),
            UploadOutcome::NotFound(message) => Err(StageUploadError::NotFound(message.clone())),
            UploadOutcome::Forbidden(message) => Err(StageUploadError::Forbidden(message.clone())),
            UploadOutcome::Invalid(message) => Err(StageUploadError::Invalid(message.clone())),
            UploadOutcome::Timeout(message) => Err(StageUploadError::Timeout(message.clone())),
            UploadOutcome::Internal(message) => Err(StageUploadError::Internal(message.clone())),
        }
    }

    async fn delete_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<(), StageUploadError> {
        self.deletes.lock().unwrap().push((account_id, upload_id));
        match &self.outcome {
            UploadOutcome::Ok => Ok(()),
            UploadOutcome::TooLarge { cap } => Err(StageUploadError::TooLarge { cap: *cap }),
            UploadOutcome::NotFound(message) => Err(StageUploadError::NotFound(message.clone())),
            UploadOutcome::Forbidden(message) => Err(StageUploadError::Forbidden(message.clone())),
            UploadOutcome::Invalid(message) => Err(StageUploadError::Invalid(message.clone())),
            UploadOutcome::Timeout(message) => Err(StageUploadError::Timeout(message.clone())),
            UploadOutcome::Internal(message) => Err(StageUploadError::Internal(message.clone())),
        }
    }

    async fn claim_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<axon_api::ClaimedUpload, StageUploadError> {
        self.claims.lock().unwrap().push((account_id, upload_id));
        match &self.outcome {
            UploadOutcome::Ok => Ok(axon_api::ClaimedUpload {
                upload_id,
                kind: axon_api::MediaUploadKindDto::Image,
                filename: "photo.png".to_owned(),
                content_type: Some("image/png".to_owned()),
                size_bytes: 3,
                bytes: b"abc".to_vec(),
            }),
            UploadOutcome::TooLarge { cap } => Err(StageUploadError::TooLarge { cap: *cap }),
            UploadOutcome::NotFound(message) => Err(StageUploadError::NotFound(message.clone())),
            UploadOutcome::Forbidden(message) => Err(StageUploadError::Forbidden(message.clone())),
            UploadOutcome::Invalid(message) => Err(StageUploadError::Invalid(message.clone())),
            UploadOutcome::Timeout(message) => Err(StageUploadError::Timeout(message.clone())),
            UploadOutcome::Internal(message) => Err(StageUploadError::Internal(message.clone())),
        }
    }

    async fn complete_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<(), StageUploadError> {
        self.completes.lock().unwrap().push((account_id, upload_id));
        match &self.outcome {
            UploadOutcome::Ok => Ok(()),
            UploadOutcome::TooLarge { cap } => Err(StageUploadError::TooLarge { cap: *cap }),
            UploadOutcome::NotFound(message) => Err(StageUploadError::NotFound(message.clone())),
            UploadOutcome::Forbidden(message) => Err(StageUploadError::Forbidden(message.clone())),
            UploadOutcome::Invalid(message) => Err(StageUploadError::Invalid(message.clone())),
            UploadOutcome::Timeout(message) => Err(StageUploadError::Timeout(message.clone())),
            UploadOutcome::Internal(message) => Err(StageUploadError::Internal(message.clone())),
        }
    }

    async fn release_upload(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
    ) -> Result<(), StageUploadError> {
        self.releases.lock().unwrap().push((account_id, upload_id));
        match &self.outcome {
            UploadOutcome::Ok => Ok(()),
            UploadOutcome::TooLarge { cap } => Err(StageUploadError::TooLarge { cap: *cap }),
            UploadOutcome::NotFound(message) => Err(StageUploadError::NotFound(message.clone())),
            UploadOutcome::Forbidden(message) => Err(StageUploadError::Forbidden(message.clone())),
            UploadOutcome::Invalid(message) => Err(StageUploadError::Invalid(message.clone())),
            UploadOutcome::Timeout(message) => Err(StageUploadError::Timeout(message.clone())),
            UploadOutcome::Internal(message) => Err(StageUploadError::Internal(message.clone())),
        }
    }
}

#[async_trait]
impl MessageSender for StubSender {
    async fn send_message(
        &self,
        account_id: Uuid,
        room_id: &str,
        body: &str,
        formatted: Option<Formatted<'_>>,
        relation: Relation<'_>,
    ) -> Result<String, SendError> {
        self.calls.lock().unwrap().push(Call::Send {
            account_id,
            room_id: room_id.to_owned(),
            body: body.to_owned(),
            formatted: formatted.map(|f| (f.format.to_owned(), f.body.to_owned())),
            reply_to: relation.reply_to.map(str::to_owned),
            thread_root: relation.thread_root.map(str::to_owned),
        });
        self.outcome.to_result()
    }

    async fn edit(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        body: &str,
        formatted: Option<Formatted<'_>>,
    ) -> Result<String, SendError> {
        self.calls.lock().unwrap().push(Call::Edit {
            account_id,
            room_id: room_id.to_owned(),
            event_id: event_id.to_owned(),
            body: body.to_owned(),
            formatted: formatted.map(|f| (f.format.to_owned(), f.body.to_owned())),
        });
        self.outcome.to_result()
    }

    async fn send_media(
        &self,
        account_id: Uuid,
        room_id: &str,
        attachment: MediaAttachment,
        caption: Option<&str>,
        relation: Relation<'_>,
    ) -> Result<String, SendError> {
        self.calls.lock().unwrap().push(Call::SendMedia {
            account_id,
            room_id: room_id.to_owned(),
            attachment,
            caption: caption.map(str::to_owned),
            reply_to: relation.reply_to.map(str::to_owned),
            thread_root: relation.thread_root.map(str::to_owned),
        });
        self.outcome.to_result()
    }

    async fn redact(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        reason: Option<&str>,
    ) -> Result<String, SendError> {
        self.calls.lock().unwrap().push(Call::Redact {
            account_id,
            room_id: room_id.to_owned(),
            event_id: event_id.to_owned(),
            reason: reason.map(str::to_owned),
        });
        self.outcome.to_result()
    }

    async fn react(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        key: &str,
    ) -> Result<String, SendError> {
        self.calls.lock().unwrap().push(Call::React {
            account_id,
            room_id: room_id.to_owned(),
            event_id: event_id.to_owned(),
            key: key.to_owned(),
        });
        self.outcome.to_result()
    }
}

/// An in-memory [`SenderTrustService`] for tests: returns a preset bundle (or
/// error), so the verification-bundle route can be exercised (auth gate, path
/// decoding, error → status mapping, DTO shape) without a real client.
pub struct StubTrust {
    bundle: Option<TrustBundle>,
    error: Option<fn() -> TrustError>,
}

impl StubTrust {
    /// A stub returning a representative bundle: a `verified` at-decrypt snapshot
    /// paired with a currently-cross-signed, verified sender.
    pub fn ok() -> Self {
        Self {
            bundle: Some(TrustBundle {
                sender: "@bob:localhost".to_owned(),
                snapshot: Some(TrustSnapshot {
                    sender_trust: Some("verified".to_owned()),
                    verification_state: Some("verified".to_owned()),
                    device_id: Some("BOBDEVICE".to_owned()),
                    curve25519_key: Some("CURVE".to_owned()),
                    ed25519_key: Some("ED".to_owned()),
                    session_id: Some("session-1".to_owned()),
                    forwarded: Some(false),
                    forwarder_user_id: None,
                    forwarder_device_id: None,
                }),
                current: CurrentTrust {
                    device_known: true,
                    device_cross_signed: Some(true),
                    identity_known: true,
                    identity_verified: Some(true),
                    verification_violation: Some(false),
                    previously_verified: Some(true),
                    master_key: Some("MASTER".to_owned()),
                },
            }),
            error: None,
        }
    }

    /// A stub returning the given error for every call.
    pub fn failing(error: fn() -> TrustError) -> Self {
        Self {
            bundle: None,
            error: Some(error),
        }
    }
}

#[async_trait]
impl SenderTrustService for StubTrust {
    async fn bundle(&self, _account_id: Uuid, _event_id: &str) -> Result<TrustBundle, TrustError> {
        if let Some(make) = self.error {
            return Err(make());
        }
        Ok(self.bundle.clone().expect("ok stub has a bundle"))
    }
}

/// An in-memory [`DeviceListService`] for tests: returns a preset device list
/// (or a canned error) and records the `user_id` each call received, so the
/// `GET …/devices` handler can be exercised — the own-user default, the
/// `?user_id=` passthrough, and error → status mapping — without a real SDK
/// client.
pub struct StubDeviceList {
    list: Option<DeviceList>,
    error: Option<fn() -> DeviceListError>,
    calls: Mutex<Vec<Option<String>>>,
}

impl StubDeviceList {
    /// A stub returning a representative single-device list for
    /// `"@alice:localhost"` (the account's own user in these tests).
    pub fn ok() -> Self {
        Self {
            list: Some(DeviceList {
                user_id: "@alice:localhost".to_owned(),
                devices: vec![DeviceInfo {
                    device_id: "ALICEDEVICE".to_owned(),
                    display_name: Some("Alice's Phone".to_owned()),
                    is_verified: false,
                    is_cross_signed_by_owner: false,
                    local_trust_state: "unset".to_owned(),
                    algorithms: vec!["m.megolm.v1.aes-sha2".to_owned()],
                }],
            }),
            error: None,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub returning the given error for every call.
    pub fn failing(error: fn() -> DeviceListError) -> Self {
        Self {
            list: None,
            error: Some(error),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub returning an empty device list for `user_id` — the
    /// unknown-but-syntactically-valid-user case, which is `200` with
    /// `devices: []`, not a `404`.
    pub fn empty(user_id: &str) -> Self {
        Self {
            list: Some(DeviceList {
                user_id: user_id.to_owned(),
                devices: vec![],
            }),
            error: None,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// The `user_id` argument of each call received so far, in order.
    pub fn calls(&self) -> Vec<Option<String>> {
        self.calls.lock().expect("lock").clone()
    }
}

#[async_trait]
impl DeviceListService for StubDeviceList {
    async fn list(
        &self,
        _account_id: Uuid,
        user_id: Option<&str>,
    ) -> Result<DeviceList, DeviceListError> {
        self.calls
            .lock()
            .expect("lock")
            .push(user_id.map(str::to_owned));
        if let Some(make) = self.error {
            return Err(make());
        }
        Ok(self.list.clone().expect("ok stub has a list"))
    }
}

/// An in-memory [`MemberProfileService`] for tests.
pub struct StubMemberProfiles {
    profiles: Vec<MemberProfile>,
    calls: Mutex<Vec<(String, Vec<String>)>>,
}

impl StubMemberProfiles {
    pub fn new(profiles: Vec<MemberProfile>) -> Self {
        Self {
            profiles,
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<(String, Vec<String>)> {
        self.calls.lock().expect("lock").clone()
    }
}

#[async_trait]
impl MemberProfileService for StubMemberProfiles {
    async fn profiles(
        &self,
        _account_id: Uuid,
        room_id: &str,
        user_ids: &[String],
    ) -> Result<Vec<MemberProfile>, MemberProfileError> {
        self.calls
            .lock()
            .expect("lock")
            .push((room_id.to_owned(), user_ids.to_vec()));
        Ok(self.profiles.clone())
    }
}

/// The outcome [`StubSearchQuery`] returns for every call.
#[derive(Clone)]
pub enum SearchOutcome {
    /// A page of hits plus the total match count across all pages.
    Hits { hits: Vec<SearchHit>, total: usize },
    /// A query-parse failure (→ `400`).
    BadQuery(String),
}

/// An in-memory [`SearchQuery`] for tests: returns a preset page of hits (or a
/// `BadQuery`) and records the params it received, so the `/v1/search` handler can
/// be exercised — filter/limit/offset passthrough, hydration from the store,
/// pagination, and error → status mapping — without a real Tantivy index.
pub struct StubSearchQuery {
    outcome: SearchOutcome,
    calls: Mutex<Vec<SearchQueryParams>>,
}

impl StubSearchQuery {
    /// A stub returning the given hits and total for every query.
    pub fn returning(hits: Vec<SearchHit>, total: usize) -> Self {
        Self {
            outcome: SearchOutcome::Hits { hits, total },
            calls: Mutex::new(Vec::new()),
        }
    }

    /// A stub that fails every query with a `BadQuery`.
    pub fn bad_query(message: &str) -> Self {
        Self {
            outcome: SearchOutcome::BadQuery(message.to_owned()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Build a hit from its `(account_id, event_id, score)` parts.
    pub fn hit(account_id: Uuid, event_id: &str, score: f32) -> SearchHit {
        SearchHit {
            account_id,
            event_id: event_id.to_owned(),
            score,
        }
    }

    /// The params of every recorded call, in order.
    pub fn calls(&self) -> Vec<SearchQueryParams> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl SearchQuery for StubSearchQuery {
    async fn search(&self, params: &SearchQueryParams) -> Result<SearchHits, SearchQueryError> {
        self.calls.lock().unwrap().push(params.clone());
        match &self.outcome {
            SearchOutcome::Hits { hits, total } => Ok(SearchHits {
                hits: hits.clone(),
                total: *total,
            }),
            SearchOutcome::BadQuery(message) => Err(SearchQueryError::BadQuery(message.clone())),
        }
    }
}

/// An in-memory [`SyncStateProvider`] for tests (ADR 0030, issue #241):
/// returns a fixed readiness label for every account, so the accounts routes'
/// `sync_state` wiring can be exercised without a real sync engine.
pub struct StubSyncState(pub &'static str);

impl SyncStateProvider for StubSyncState {
    fn sync_state(&self, _account_id: Uuid) -> &'static str {
        self.0
    }
}
