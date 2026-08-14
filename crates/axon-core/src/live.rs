//! The live-event bus payload.
//!
//! [`LiveFrame`] is what the sync engine publishes onto the live-event bus, and
//! what the `/v1/ws` WebSocket handler fans out to connected clients. It is an
//! enum so one broadcast channel carries every kind of live frame — timeline
//! events ([`LiveEvent`]) today, and interactive-verification frames
//! ([`VerificationFrame`]) — without a second channel or lag domain. The wire
//! `type` tag that distinguishes them on the socket is owned by `axon-api`.
//!
//! These types are deliberately **wire-neutral**: they carry the fields the
//! read/WS API needs, but the HTTP/WebSocket envelope shape is owned by
//! `axon-api`. Keeping them here — the lowest crate — lets the two sibling
//! crates (`axon-sync` produces, `axon-api` consumes) share them without either
//! depending on the other.

use serde_json::Value;
use uuid::Uuid;

/// Everything the live-event bus carries. A single
/// [`tokio::sync::broadcast`](https://docs.rs/tokio/latest/tokio/sync/broadcast)
/// channel of these fans out to every `/v1/ws` subscriber, so all live traffic
/// shares one ordering and one lag signal. `Clone` is required because the
/// channel clones each message to every receiver.
#[derive(Debug, Clone)]
pub enum LiveFrame {
    /// A freshly persisted timeline event.
    Timeline(LiveEvent),
    /// A state change in an interactive (SAS) device-verification flow.
    Verification(VerificationFrame),
    /// A sender's *current* device trust changed (M7c) — e.g. their identity
    /// entered a verification violation. An overlay distinct from the immutable
    /// per-event snapshot: it names the affected sender so clients re-evaluate
    /// (re-read the bundle / timeline), it does not carry per-event diffs.
    SenderTrustChanged(SenderTrustFrame),
    /// Per-device client state changed (M12) — a device PUT drafts / read
    /// markers, and sibling devices should apply the change.
    DeviceState(DeviceStateFrame),
    /// A raw, allowlisted ephemeral event forwarded verbatim (ADR 0056) — e.g.
    /// `m.typing`, `m.receipt`. Axon adds no value to these; unlike the other
    /// variants above, this is the generic escape hatch for the long tail of
    /// ephemeral signals that don't warrant a bespoke frame.
    Ephemeral(EphemeralFrame),
    /// A room's SDK-derived unread counts changed (issue #313, ADR 0070) —
    /// matrix-sdk's read-receipt-based notification/mention counters for the
    /// room. Unlike [`EphemeralFrame`] this is *not* a raw event passthrough:
    /// it is not built on the ADR 0056 ephemeral path at all, since unread
    /// counts are room state, not an ephemeral event.
    UnreadCountsChanged(UnreadCountsFrame),
    /// An account's sync-engine readiness transitioned (ADR 0030, issue
    /// #241) — `"connecting"` / `"syncing"` / `"ready"` / `"offline"`. Lets a
    /// connected client update its per-account status without polling
    /// `GET /v1/accounts`.
    SyncStateChanged(SyncStateFrame),
    /// A pending invite was added or its display fields changed (ADR 0091).
    InviteAdded(InviteAddedFrame),
    /// A pending invite is gone — accepted, rejected, or withdrawn (ADR 0091).
    InviteRemoved(InviteRemovedFrame),
}

impl From<LiveEvent> for LiveFrame {
    fn from(event: LiveEvent) -> Self {
        LiveFrame::Timeline(event)
    }
}

impl From<VerificationFrame> for LiveFrame {
    fn from(frame: VerificationFrame) -> Self {
        LiveFrame::Verification(frame)
    }
}

impl From<SenderTrustFrame> for LiveFrame {
    fn from(frame: SenderTrustFrame) -> Self {
        LiveFrame::SenderTrustChanged(frame)
    }
}

impl From<DeviceStateFrame> for LiveFrame {
    fn from(frame: DeviceStateFrame) -> Self {
        LiveFrame::DeviceState(frame)
    }
}

impl From<EphemeralFrame> for LiveFrame {
    fn from(frame: EphemeralFrame) -> Self {
        LiveFrame::Ephemeral(frame)
    }
}

impl From<UnreadCountsFrame> for LiveFrame {
    fn from(frame: UnreadCountsFrame) -> Self {
        LiveFrame::UnreadCountsChanged(frame)
    }
}

impl From<SyncStateFrame> for LiveFrame {
    fn from(frame: SyncStateFrame) -> Self {
        LiveFrame::SyncStateChanged(frame)
    }
}

impl From<InviteAddedFrame> for LiveFrame {
    fn from(frame: InviteAddedFrame) -> Self {
        LiveFrame::InviteAdded(frame)
    }
}

impl From<InviteRemovedFrame> for LiveFrame {
    fn from(frame: InviteRemovedFrame) -> Self {
        LiveFrame::InviteRemoved(frame)
    }
}

/// A per-device state change (M12, ADR 0048), ready to fan out over the
/// live-event bus. Carries the written entries themselves so sibling devices
/// apply the change without a read-back; the bus is lossy, so a reconnecting
/// client re-reads the merged view via `GET …/state/{namespace}` instead of
/// assuming the frames it missed. `device_id` names the *originator*: the bus
/// is a single global broadcast, so clients drop frames carrying their own
/// device id (echo suppression), exactly as they already self-filter by
/// `account_id`.
#[derive(Debug, Clone)]
pub struct DeviceStateFrame {
    /// Axon account this state belongs to.
    pub account_id: Uuid,
    /// The device that wrote the change (client-supplied UUID) — receivers
    /// matching this id ignore the frame.
    pub device_id: Uuid,
    /// The namespace the entries were written under, e.g. `drafts`.
    pub namespace: String,
    /// The written `(key, value)` pairs; a `None` value is a deletion.
    pub entries: Vec<(String, Option<Value>)>,
    /// Server-clock write time (the last-write-wins ordering).
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A raw, allowlisted ephemeral event (ADR 0056), ready to fan out over the
/// live-event bus unmodified. This is the generic passthrough for the
/// ephemeral long tail — `m.typing`, `m.receipt`, and future allowlisted
/// types — that Axon never persists and derives nothing from: it forwards the
/// event's `type` and `content` verbatim rather than reshaping them into a
/// typed DTO. A signal graduates from this generic frame to a bespoke one
/// (like [`SenderTrustFrame`] or [`VerificationFrame`]) the day Axon starts
/// deriving something from it, not before.
#[derive(Debug, Clone)]
pub struct EphemeralFrame {
    /// Axon account this event belongs to.
    pub account_id: Uuid,
    /// Matrix room ID the event is scoped to. `None` for account-scoped
    /// signals (e.g. presence) — currently unused by any production
    /// constructor, since forwarding an account-scoped signal needs its own
    /// handler registration, not just a value here; kept `Option` so that
    /// future handler's wire shape doesn't need a breaking change.
    pub room_id: Option<String>,
    /// Matrix event type, e.g. `"m.typing"`, `"m.receipt"`.
    pub event_type: String,
    /// The raw event `content`, unmodified.
    pub content: Value,
}

/// A room's SDK-derived unread counts changed (issue #313, ADR 0070), ready
/// to fan out over the live-event bus. `notification_count`/`highlight_count`
/// are matrix-sdk's client-side read-receipt-derived
/// `Room::num_unread_notifications()` and `Room::num_unread_mentions()`
/// values; Axon only caches and republishes them.
#[derive(Debug, Clone)]
pub struct UnreadCountsFrame {
    /// Axon account this room belongs to.
    pub account_id: Uuid,
    /// Matrix room ID.
    pub room_id: String,
    /// Total unread notification count.
    pub notification_count: u64,
    /// The subset of `notification_count` that is an unread mention.
    pub highlight_count: u64,
}

/// An account's sync-engine readiness transition (ADR 0030, issue #241), ready
/// to fan out over the live-event bus. `sync_state` is one of `"connecting"`,
/// `"syncing"`, `"ready"`, `"offline"` — the same closed vocabulary as
/// `AccountDto.sync_state`. Carried as a plain string rather than a shared enum
/// so this crate (the lowest one, shared by both `axon-sync` and `axon-api`)
/// doesn't need to own the wire vocabulary; `axon-api` maps it onto its own
/// `SyncStateDto` for the WS payload.
#[derive(Debug, Clone)]
pub struct SyncStateFrame {
    /// Axon account whose sync readiness changed.
    pub account_id: Uuid,
    /// The new state: `"connecting"`, `"syncing"`, `"ready"`, or `"offline"`.
    pub sync_state: &'static str,
}

/// A pending invite appeared or its display snapshot changed (ADR 0091).
/// Payload matches `GET /v1/invites` so a connected client can upsert
/// without a read-back. The bus is lossy; a reconnecting client re-reads
/// the list.
#[derive(Debug, Clone)]
pub struct InviteAddedFrame {
    pub account_id: Uuid,
    pub account_user_id: String,
    pub room_id: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub topic: Option<String>,
    pub canonical_alias: Option<String>,
    pub room_type: Option<String>,
    pub inviter_user_id: String,
    pub inviter_display_name: Option<String>,
    pub is_direct: bool,
    pub encrypted: bool,
    pub invited_at: chrono::DateTime<chrono::Utc>,
}

/// A pending invite is no longer pending (ADR 0091).
#[derive(Debug, Clone)]
pub struct InviteRemovedFrame {
    pub account_id: Uuid,
    pub room_id: String,
}

/// A change in a *sender's* current device trust (M7c), ready to fan out over the
/// live-event bus. Deliberately coarse: it names the sender whose trust changed
/// (and the new violation state) so a client can re-evaluate that sender's
/// messages by re-reading the timeline / verification bundle — the per-event
/// snapshot the read API returns is the source of truth, this is only the push
/// notification that it's worth re-reading.
#[derive(Debug, Clone)]
pub struct SenderTrustFrame {
    /// Axon account whose view this change is in.
    pub account_id: Uuid,
    /// Matrix user id of the sender whose current trust changed.
    pub user_id: String,
    /// Whether the sender's identity is now in a verification violation
    /// (previously verified, identity since changed).
    pub verification_violation: bool,
}

/// A state change in an interactive SAS verification flow, ready to fan out over
/// the live-event bus. Carries everything a client needs to render the flow's
/// new stage without an out-of-band read — though the same values stay
/// re-readable via `GET …/verify/{flow_id}` while the flow is live, which is how
/// a reconnecting client that missed a frame recovers (see ADR 0011 / M7a PR6).
#[derive(Debug, Clone)]
pub struct VerificationFrame {
    /// Axon account this flow belongs to.
    pub account_id: Uuid,
    /// The verification transaction id, stable for the life of the flow.
    pub flow_id: String,
    /// Which stage this frame reports.
    pub kind: VerificationFrameKind,
    /// The user whose identity/device is being verified. For self-verification
    /// this is the account's own user id; for cross-user verification (ADR 0040)
    /// it is the peer's user id.
    pub target_user_id: String,
    /// The other device in the flow for self-verification. Cross-user
    /// verification targets an identity rather than one known device.
    pub target_device_id: Option<String>,
    /// The SAS emoji as `(symbol, description)` pairs — present once keys are
    /// exchanged (a [`VerificationFrameKind::Sas`] frame).
    pub emoji: Option<Vec<(String, String)>>,
    /// The SAS decimal triple — the alternative representation of the same SAS,
    /// present alongside `emoji`.
    pub decimals: Option<(u16, u16, u16)>,
    /// A human-readable outcome — the cancel reason for a
    /// [`VerificationFrameKind::Cancelled`] frame.
    pub outcome: Option<String>,
}

/// Which stage of a verification flow a [`VerificationFrame`] reports. Maps 1:1
/// to the `verification.*` wire `type` tag in `axon-api`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationFrameKind {
    /// A verification was requested (including peer-initiated requests, which
    /// have no HTTP kickoff).
    Requested,
    /// The SAS is ready to compare — `emoji`/`decimals` are populated.
    Sas,
    /// The flow completed successfully; the device is now cross-signed.
    Done,
    /// The flow was cancelled (timeout, mismatch, or either side cancelling).
    Cancelled,
}

/// A timeline event freshly persisted by the sync engine, ready to fan out over
/// the live-event bus. `Clone` is required because it travels a
/// [`tokio::sync::broadcast`](https://docs.rs/tokio/latest/tokio/sync/broadcast)
/// channel, which clones each message to every receiver.
///
/// Fields mirror the read API's event shape. A live event is never
/// already-redacted at arrival (a redaction is a separate event that arrives
/// later), so there is no redaction state here — the API maps it to a
/// non-redacted DTO.
#[derive(Debug, Clone)]
pub struct LiveEvent {
    /// Axon account this event belongs to.
    pub account_id: Uuid,
    /// Matrix event ID.
    pub event_id: String,
    /// Matrix room ID.
    pub room_id: String,
    /// Matrix user ID of the sender.
    pub sender: String,
    /// Matrix state key for state events. `None` for message-like events.
    pub state_key: Option<String>,
    /// The `unsigned.prev_content` of a state event — the state content this
    /// event replaced, e.g. the previous `m.room.member` membership/displayname
    /// (issue #31). `None` for message-like events and for state events with no
    /// prior state (e.g. room creation).
    pub prev_content: Option<Value>,
    /// `origin_server_ts` in milliseconds.
    pub origin_ts: i64,
    /// The event's arrival order: the monotonic position this account's store
    /// assigned it on ingestion (`events.id`). Carried on the live bus so a
    /// `/v1/ws` frame states it as authoritatively as a timeline read does —
    /// clients pick their read-receipt target by this, not by `origin_ts`
    /// (ADR 0089), and a frame that omitted it would force them to infer one.
    pub arrival_order: i64,
    /// Matrix event type, e.g. `m.room.message`.
    pub event_type: String,
    /// Decrypted `content` JSON. `None` for events that arrived as UTDs.
    pub content: Option<Value>,
    /// Plaintext body, when the content carried one.
    pub body: Option<String>,
    /// The event's `m.relates_to` object, if any.
    pub relates_to: Option<Value>,
    /// The sender-trust verdict snapshot (M7c): `verified`, `unverified`,
    /// `unknown`, or `verification_violation`. `None` for unencrypted events and
    /// for UTDs (whose verdict is only known once re-decryption back-fills it —
    /// re-decryption does not re-emit a live frame, so clients re-read the
    /// timeline for the back-filled value).
    pub sender_trust: Option<String>,
}
