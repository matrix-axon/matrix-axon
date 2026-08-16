use ratatui::layout::{Rect, Size};
use ratatui_image::picker::Picker;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Semaphore};
use uuid::Uuid;

#[cfg(test)]
use crate::api::LiveFrame;
use crate::api::{
    AccountDto, AccountState, AxonClient, EmojiDto, EventDto, FlowDto, FlowStage, RoomDto,
    SendRelation, VerificationFrameDto, VerificationFrameKind,
};
use crate::command::Command;
#[cfg(test)]
use crate::config::MessageDensity;
use crate::config::{ColorScheme, DisplayOptions, Shortcuts, TuiConfig};
use crate::html::{markdown_to_html_if_detected, rainbow_html, spoiler_html, strip_html_to_plain};
use crate::search::{SearchFormState, SearchRequest, SearchResultsState};
#[cfg(test)]
use ratatui::style::Modifier;
use std::path::PathBuf;
mod completion;
mod drafts;
mod ephemeral;
pub(crate) use drafts::{load_or_create_device_id, DraftOutcome};
mod lifecycle;
mod read_markers;
pub(crate) use lifecycle::LifecycleOutcome;
pub(crate) mod media;
pub(crate) use media::{
    ImageState, MediaKey, MediaResult, ProtocolKey, ProtocolState, IMAGE_CACHE_LIMIT,
    MEDIA_WORKERS, PROTOCOL_CACHE_LIMIT,
};
mod reactions;
mod relations;
mod render;
mod room_actions;
mod rooms;
mod search_flow;
mod timeline;
mod typing;

pub(crate) use reactions::{collect_reactions, emoji_matches, unreact_selection_status};
pub(crate) use render::{
    date_separator_line, display_body_with_sender, format_date, format_time, message_index_at_line,
    message_layout, selected_line_style, ImageThumbRows, RelationContext, ReplyPreview,
    ThreadBadge, IMAGE_THUMB_ROWS,
};
pub(crate) use room_actions::{PendingRoomAction, RoomActionOutcome};
pub(crate) use rooms::{account_localpart, apply_edits, dm_title_from_members};
#[cfg(test)]
use timeline::should_show_event;

/// The outcome of an [`App::request_protocol`] call.
///
/// The function used to return `()`, so a caller could not tell an encode that
/// started from one silently dropped (#51).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProtocolRequest {
    /// An encode was spawned.
    Started,
    /// Already encoded, or already encoding — nothing more to do.
    AlreadyPresent,
    /// Zero width or height; there is nothing to encode into.
    EmptySize,
    /// The decoded image has not arrived yet. Expected, and self-correcting.
    ImageNotReady,
    /// Every protocol-cache slot is mid-encode, so no slot could be freed.
    CacheSaturated,
    /// The media channel was never wired, so no encode can ever complete.
    ChannelUnwired,
}

/// Counts of [`App::request_protocol`] calls that could not start an encode,
/// surfaced in the `display.debug` overlay.
///
/// Faults only: a request still waiting on its image is the normal path, not a
/// drop, and is deliberately not counted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProtocolDropCounts {
    /// Bumped when the protocol cache is entirely `Encoding`.
    pub(crate) cache_saturated: u64,
    /// Bumped when `media_tx` is unset — a wiring bug, not a transient.
    pub(crate) channel_unwired: u64,
}

/// How long a Sixel image is left alone before its pixels are retransmitted.
///
/// tmux does not retain the pixel data behind a pane, so a Sixel image that
/// scrolls, is uncovered, or simply sits there can lose it. Bumping a
/// generation counter changes the encoded payload's trailing SGR, which is
/// enough to make ratatui's cell diff re-emit the image.
pub(crate) const SIXEL_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

const TIMELINE_LIMIT: usize = 50;
pub(super) const PENDING_ECHO_MSG: &str =
    "message not yet confirmed by server — please wait a moment and try again";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mode {
    Compose,
    LoginUsername,
    LoginPassword {
        username: String,
        /// Homeserver override carried from the username step, if the user gave
        /// one there. `None` means Axon resolves the homeserver.
        homeserver: Option<String>,
    },
    RecoveryKey {
        account: AccountDto,
        origin: RecoveryOrigin,
    },
    ConfirmLogout {
        account: AccountDto,
    },
    ConfirmDelete {
        account: AccountDto,
    },
    ConfirmRoomAction {
        action: PendingRoomAction,
    },
    RoomList,
    AccountList,
    MessageList,
    Search(SearchKind, String),
    SearchForm,
    SearchResults,
    Editing {
        event_id: String,
    },
    Reacting {
        event_id: String,
    },
    Unreacting {
        target_event_id: String,
        choices: Vec<OwnReaction>,
        selected: usize,
    },
    /// The SAS emoji verification modal (ADR 0028). Its own mode with literal
    /// `y`/`n`/`Esc` keys; the live flow state lives in `App::verification`.
    Verification,
    Popup(PopupKind),
    /// Date-jump input: user types a date string; Enter jumps the current room's
    /// timeline to that date, Esc returns to MessageList.
    DateJump,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryOrigin {
    PostLogin,
    Command,
}

/// Which side started the SAS flow (ADR 0028 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerificationDirection {
    /// Peer-initiated: surfaced via a `verification.requested` frame.
    Incoming,
    /// TUI-initiated via `/verify <device_id>`.
    Outgoing,
}

/// The UI stage of the verification modal. Distinct from the server `FlowStage`
/// because it also represents pre-start (`Starting`) and terminal-with-message
/// (`Done`/`Ended`) display states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VerificationStage {
    /// Outgoing start request dispatched; awaiting a `flow_id`.
    Starting,
    /// Flow open, SAS not yet available (server `requested`/`ready`).
    Waiting,
    /// SAS emoji available; awaiting the user's `y`/`n`.
    Compare,
    /// The user confirmed; awaiting the peer / `verification.done`.
    Confirming,
    /// Terminal success — awaiting `Esc` to dismiss.
    Done,
    /// Terminal cancel/error with a message — awaiting `Esc` to dismiss.
    Ended(String),
}

impl VerificationStage {
    /// Whether the flow has reached a terminal state the user must dismiss.
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Ended(_))
    }
}

/// Live state for the verification modal. One flow at a time (concurrent flows
/// are not expected).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerificationFlow {
    pub(crate) account_id: Uuid,
    /// The user being verified — own user id (self-verification) or the peer's
    /// (cross-user, ADR 0040). Empty if the server didn't report it.
    pub(crate) user_id: String,
    pub(crate) device_id: String,
    /// `None` only between an outgoing start request and its `flow_id` response.
    pub(crate) flow_id: Option<String>,
    pub(crate) direction: VerificationDirection,
    pub(crate) stage: VerificationStage,
    pub(crate) emoji: Option<Vec<EmojiDto>>,
    pub(crate) decimals: Option<[u16; 3]>,
}

impl VerificationFlow {
    /// Whether this flow matches a frame/response for `account_id` + `flow_id`.
    pub(crate) fn matches(&self, account_id: Uuid, flow_id: &str) -> bool {
        self.account_id == account_id && self.flow_id.as_deref() == Some(flow_id)
    }

    /// Whether a frame could be the server echo for an outgoing verification
    /// whose `POST .../verify` response has not returned yet.
    pub(crate) fn is_pending_outgoing_target(
        &self,
        account_id: Uuid,
        user_id: &str,
        device_id: Option<&str>,
    ) -> bool {
        if self.account_id != account_id
            || self.flow_id.is_some()
            || self.direction != VerificationDirection::Outgoing
        {
            return false;
        }
        device_id.is_some_and(|device_id| !device_id.is_empty() && self.device_id == device_id)
            || (!user_id.is_empty() && self.user_id == user_id)
    }

    /// Whether a non-request frame can safely bind this pending outgoing flow to
    /// the server flow id. User ids are not unique per verification flow, so only
    /// a device id is specific enough to adopt here.
    pub(crate) fn is_pending_outgoing_device(
        &self,
        account_id: Uuid,
        device_id: Option<&str>,
    ) -> bool {
        self.account_id == account_id
            && self.flow_id.is_none()
            && self.direction == VerificationDirection::Outgoing
            && device_id
                .is_some_and(|device_id| !device_id.is_empty() && self.device_id == device_id)
    }

    /// Advance the UI stage from a server `FlowStage`, given whether SAS emoji
    /// are present. Terminal states preserve any already-shown message.
    fn stage_from(
        stage: FlowStage,
        has_emoji: bool,
        cancel_reason: Option<&str>,
    ) -> VerificationStage {
        match stage {
            FlowStage::Requested | FlowStage::Ready => VerificationStage::Waiting,
            FlowStage::KeysExchanged => {
                if has_emoji {
                    VerificationStage::Compare
                } else {
                    VerificationStage::Waiting
                }
            }
            FlowStage::Confirmed => VerificationStage::Confirming,
            FlowStage::Done => VerificationStage::Done,
            FlowStage::Cancelled => VerificationStage::Ended(format!(
                "Verification cancelled{}",
                cancel_reason.map(|r| format!(" — {r}")).unwrap_or_default()
            )),
        }
    }

    /// Merge an authoritative `FlowDto` (a read-on-reconnect resync or a list
    /// entry) into the flow state.
    pub(crate) fn apply_flow(&mut self, flow: &FlowDto) {
        if self.user_id.is_empty() && !flow.user_id.is_empty() {
            self.user_id = flow.user_id.clone();
        }
        if let Some(device_id) = &flow.device_id {
            if self.device_id.is_empty() {
                self.device_id = device_id.clone();
            }
        }
        if flow.emoji.is_some() {
            self.emoji = flow.emoji.clone();
        }
        if flow.decimals.is_some() {
            self.decimals = flow.decimals;
        }
        // Don't regress out of a local Confirming state into Compare just
        // because the server hasn't recorded our confirm yet.
        let next = Self::stage_from(
            flow.stage,
            self.emoji.is_some(),
            flow.cancel_reason.as_deref(),
        );
        if !(self.stage == VerificationStage::Confirming && next == VerificationStage::Compare) {
            self.stage = next;
        }
    }

    /// Merge a live `verification.*` frame into the flow state.
    pub(crate) fn apply_frame(
        &mut self,
        kind: VerificationFrameKind,
        payload: &VerificationFrameDto,
    ) {
        if self.flow_id.is_none() {
            self.flow_id = Some(payload.flow_id.clone());
        }
        // Adopt identity fields the server reports once known (a self-verification
        // flow learns its own user id from frames; a cross-user flow learns the
        // peer's device once SAS begins).
        if self.user_id.is_empty() && !payload.user_id.is_empty() {
            self.user_id = payload.user_id.clone();
        }
        if let Some(device_id) = &payload.device_id {
            if self.device_id.is_empty() {
                self.device_id = device_id.clone();
            }
        }
        match kind {
            VerificationFrameKind::Requested => {
                if !self.stage.is_terminal() {
                    self.stage = VerificationStage::Waiting;
                }
            }
            VerificationFrameKind::Sas => {
                if payload.emoji.is_some() {
                    self.emoji = payload.emoji.clone();
                }
                if payload.decimals.is_some() {
                    self.decimals = payload.decimals;
                }
                if self.stage != VerificationStage::Confirming {
                    self.stage = VerificationStage::Compare;
                }
            }
            VerificationFrameKind::Done => self.stage = VerificationStage::Done,
            VerificationFrameKind::Cancelled => {
                self.stage = VerificationStage::Ended(format!(
                    "Verification cancelled{}",
                    payload
                        .reason
                        .as_deref()
                        .map(|r| format!(" — {r}"))
                        .unwrap_or_default()
                ));
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnReaction {
    pub(crate) key: String,
    pub(crate) event_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnreadThreadPreview {
    pub(crate) event_id: String,
    pub(crate) sender: String,
    pub(crate) body: String,
    pub(crate) origin_ts: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnreadThread {
    pub(crate) root_event_id: String,
    pub(crate) unread_count: usize,
    pub(crate) latest_event_id: String,
    pub(crate) latest_sender: String,
    pub(crate) latest_body: String,
    pub(crate) latest_ts: i64,
    pub(crate) recent: Vec<UnreadThreadPreview>,
    /// Every event id ever counted toward `unread_count`, so a reply observed
    /// twice (seen live, then re-encountered by a timeline load while still
    /// past the read marker) can't inflate the count. `recent` can't serve as
    /// the dedupe set — it is truncated to the preview budget. Session-local
    /// and cleared with the marker, so bounded by the thread's own size.
    pub(crate) counted: std::collections::HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnreadThreadEntry {
    pub(crate) room_key: RoomKey,
    pub(crate) room_title: String,
    pub(crate) root_event_id: String,
    pub(crate) root_snippet: Option<String>,
    pub(crate) unread_count: usize,
    pub(crate) latest_sender: String,
    pub(crate) latest_body: String,
    pub(crate) latest_ts: i64,
    pub(crate) recent: Vec<UnreadThreadPreview>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnreadThreadSelection {
    pub(crate) room_key: RoomKey,
    pub(crate) root_event_id: String,
}

impl From<&UnreadThreadEntry> for UnreadThreadSelection {
    fn from(entry: &UnreadThreadEntry) -> Self {
        Self {
            room_key: entry.room_key.clone(),
            root_event_id: entry.root_event_id.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchKind {
    Rooms,
    Messages,
    Accounts,
    /// Live name filter for the room list: each keystroke updates
    /// `App::room_filter` to `RoomFilter::Name(query)` (ADR 0042). Distinct from
    /// `Rooms`, which is a jump-to-match search committed on Enter.
    RoomNameFilter,
}

/// Room-list filter mode (ADR 0042). The account filter is applied separately
/// and always; this narrows within it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum RoomFilter {
    #[default]
    All,
    /// Direct messages only (interim heuristic — see [`is_likely_dm`]).
    Dms,
    /// Group (non-DM) rooms only.
    Groups,
    /// Rooms with unread messages only.
    Unread,
    /// Pinned rooms only.
    Favorites,
    /// Rooms whose name/alias/topic/id contains the (lowercased) query.
    Name(String),
}

impl RoomFilter {
    /// The filters the cycle key rotates through. `Name` is excluded — it is
    /// entered explicitly via its own text-input shortcut.
    const CYCLE: [RoomFilter; 5] = [
        RoomFilter::All,
        RoomFilter::Dms,
        RoomFilter::Groups,
        RoomFilter::Unread,
        RoomFilter::Favorites,
    ];

    /// The next filter in the cycle. A `Name` filter cycles to `All`.
    pub(crate) fn next(&self) -> RoomFilter {
        let pos = Self::CYCLE
            .iter()
            .position(|f| f == self)
            .unwrap_or(Self::CYCLE.len() - 1);
        Self::CYCLE[(pos + 1) % Self::CYCLE.len()].clone()
    }

    /// Short label for the room-list block title.
    pub(crate) fn label(&self) -> String {
        match self {
            RoomFilter::All => "All".to_owned(),
            RoomFilter::Dms => "DMs".to_owned(),
            RoomFilter::Groups => "Groups".to_owned(),
            RoomFilter::Unread => "Unread".to_owned(),
            RoomFilter::Favorites => "Favorites".to_owned(),
            RoomFilter::Name(q) => format!("Filter: {q}"),
        }
    }

    /// Config token for `[display] room_filter`. `Name` persists as `all`
    /// (a stale query string is not restored).
    pub(crate) fn as_config_str(&self) -> &'static str {
        match self {
            RoomFilter::All | RoomFilter::Name(_) => "all",
            RoomFilter::Dms => "dms",
            RoomFilter::Groups => "groups",
            RoomFilter::Unread => "unread",
            RoomFilter::Favorites => "favorites",
        }
    }

    /// Parse a config token (and the `/filter` command argument). Unknown values
    /// (other than the recognized keywords) are treated as a name filter.
    pub(crate) fn parse(s: &str) -> RoomFilter {
        match s.trim().to_lowercase().as_str() {
            "" | "all" => RoomFilter::All,
            "dms" | "dm" | "people" => RoomFilter::Dms,
            "groups" | "group" | "rooms" => RoomFilter::Groups,
            "unread" => RoomFilter::Unread,
            "fav" | "favs" | "favorites" => RoomFilter::Favorites,
            _ => RoomFilter::Name(s.trim().to_lowercase()),
        }
    }
}

/// Room-list sort mode (ADR 0042). Applies to the unpinned tail only; the pinned
/// section keeps its pin-position order (ADR 0038).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum RoomSort {
    #[default]
    RecentActivity,
    OldestActivity,
    AlphaAsc,
    AlphaDesc,
}

impl RoomSort {
    const CYCLE: [RoomSort; 4] = [
        RoomSort::RecentActivity,
        RoomSort::OldestActivity,
        RoomSort::AlphaAsc,
        RoomSort::AlphaDesc,
    ];

    pub(crate) fn next(self) -> RoomSort {
        let pos = Self::CYCLE.iter().position(|s| *s == self).unwrap_or(0);
        Self::CYCLE[(pos + 1) % Self::CYCLE.len()]
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            RoomSort::RecentActivity => "Recent",
            RoomSort::OldestActivity => "Oldest",
            RoomSort::AlphaAsc => "A–Z",
            RoomSort::AlphaDesc => "Z–A",
        }
    }

    pub(crate) fn as_config_str(self) -> &'static str {
        match self {
            RoomSort::RecentActivity => "recent",
            RoomSort::OldestActivity => "oldest",
            RoomSort::AlphaAsc => "az",
            RoomSort::AlphaDesc => "za",
        }
    }

    pub(crate) fn parse(s: &str) -> RoomSort {
        match s.trim().to_lowercase().as_str() {
            "oldest" => RoomSort::OldestActivity,
            "az" | "alpha" | "a-z" => RoomSort::AlphaAsc,
            "za" | "z-a" => RoomSort::AlphaDesc,
            _ => RoomSort::RecentActivity,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountSelection {
    All,
    Account(usize),
}

impl AccountSelection {
    pub(crate) fn display_number(self) -> usize {
        match self {
            Self::All => 0,
            Self::Account(index) => index + 1,
        }
    }

    pub(crate) fn display_label(self, user_id: Option<&str>) -> String {
        match self {
            Self::All => format!("{} All Accounts", self.display_number()),
            Self::Account(_) => {
                format!("{} {}", self.display_number(), user_id.unwrap_or("?"))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PopupKind {
    Help,
    Shortcuts,
    UnreadThreads,
    RoomInfo,
    Status,
    CommandResponse,
    MediaPreview,
}

#[derive(Debug, Clone, Default)]
pub(crate) enum ConnectionState {
    #[default]
    Unknown,
    Connected,
    Reconnecting {
        reason: String,
        delay: std::time::Duration,
    },
    Disconnected(String),
    ProtocolError(String),
}

#[derive(Debug, Clone)]
pub(crate) enum Status {
    /// Transient guidance or general operation feedback.
    Info(String),
    /// Diagnostics hidden unless debug display is enabled.
    Debug(String),
    /// Feedback for an action tied to a specific event, with identifiers hidden by default.
    EventAction {
        debug: String,
        redacted: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveFrameAction {
    None,
    RefreshRooms,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RoomTargetResolution {
    Match(usize),
    Ambiguous(Vec<String>),
    Missing,
}

impl Status {
    pub(crate) fn text(&self, debug_enabled: bool) -> String {
        match self {
            Self::Info(text) => text.clone(),
            Self::Debug(text) => {
                if debug_enabled {
                    text.clone()
                } else {
                    String::new()
                }
            }
            Self::EventAction { debug, redacted } => {
                if debug_enabled {
                    debug.clone()
                } else {
                    (*redacted).to_owned()
                }
            }
        }
    }
}

impl From<String> for Status {
    fn from(value: String) -> Self {
        Self::Info(value)
    }
}

impl From<&str> for Status {
    fn from(value: &str) -> Self {
        Self::Info(value.to_owned())
    }
}

impl PartialEq<&str> for Status {
    fn eq(&self, other: &&str) -> bool {
        self.text(true) == *other || self.text(false) == *other
    }
}

impl PartialEq<Status> for &str {
    fn eq(&self, other: &Status) -> bool {
        other == self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RoomKey {
    pub(crate) account_id: Uuid,
    pub(crate) room_id: String,
}

impl From<&RoomDto> for RoomKey {
    fn from(room: &RoomDto) -> Self {
        Self {
            account_id: room.account_id,
            room_id: room.room_id.clone(),
        }
    }
}

impl RoomKey {
    /// Parse a config entry of the form `"account_id:room_id"` (ADR 0038). The
    /// account id is a UUID, so we split on the first `:`; the remainder is the
    /// room id (which itself contains colons, e.g. `!abc:server`). Returns `None`
    /// for malformed entries so a hand-edited config can't crash startup.
    fn parse_config_entry(entry: &str) -> Option<Self> {
        let (account, room_id) = entry.split_once(':')?;
        let account_id = Uuid::parse_str(account).ok()?;
        if room_id.is_empty() {
            return None;
        }
        Some(Self {
            account_id,
            room_id: room_id.to_owned(),
        })
    }

    /// Serialize to the `"account_id:room_id"` form stored in the config file.
    fn to_config_entry(&self) -> String {
        format!("{}:{}", self.account_id, self.room_id)
    }
}

/// Largest local file `/send` will read into memory to stage for upload.
/// Mirrors `api::MAX_MEDIA_BYTES`'s reasoning for downloads (root AGENTS.md's
/// "never size a buffer or allocation directly from a number the peer
/// controls" applies equally to a user-supplied local path): without this,
/// `/send`-ing a huge or pathological file (an accidentally-dropped video, a
/// device path) would buffer the whole thing into a `Vec<u8>` unconditionally.
const MAX_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;

/// Infer a `(kind, content_type)` pair for `/send` from a filename's
/// extension (ADR 0059/0062) — good enough to satisfy the server's
/// `kind=image` ⇒ `image/*` validation, not attempting to be exhaustive.
fn media_kind_and_content_type(filename: &str) -> (&'static str, &'static str) {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "png" => ("image", "image/png"),
        "jpg" | "jpeg" => ("image", "image/jpeg"),
        "gif" => ("image", "image/gif"),
        "webp" => ("image", "image/webp"),
        "bmp" => ("image", "image/bmp"),
        "tif" | "tiff" => ("image", "image/tiff"),
        "pdf" => ("file", "application/pdf"),
        "txt" => ("file", "text/plain"),
        "zip" => ("file", "application/zip"),
        "mp4" => ("file", "video/mp4"),
        "mp3" => ("file", "audio/mpeg"),
        _ => ("file", "application/octet-stream"),
    }
}

fn expand_send_path(path: &str) -> PathBuf {
    expand_send_path_with_home(path, std::env::var_os("HOME"))
}

fn expand_send_path_with_home(path: &str, home: Option<std::ffi::OsString>) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home.filter(|value| !value.is_empty()) {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

pub(crate) struct App {
    pub(crate) client: AxonClient,
    pub(crate) account_filter: Option<Uuid>,
    pub(crate) shortcuts: Shortcuts,
    pub(crate) colors: ColorScheme,
    pub(crate) display: DisplayOptions,
    pub(crate) rooms: RoomsState,
    pub(crate) accounts: AccountsState,
    pub(crate) messages: MessagePane,
    pub(crate) input: InputState,
    pub(crate) live: LiveState,
    pub(crate) connection_state: ConnectionState,
    pub(crate) mode: Mode,
    pub(crate) popup_scroll: usize,
    pub(crate) help_selection: usize,
    pub(crate) last_search: Option<String>,
    pub(crate) last_jump_ts: Option<i64>,
    pub(crate) search_form: SearchFormState,
    pub(crate) search_results: Option<SearchResultsState>,
    /// Sender for completed `/v1/search` calls spawned off the event loop.
    /// `None` until the main loop wires up the channel (and in unit tests).
    pub(crate) search_tx: Option<mpsc::UnboundedSender<search_flow::SearchOutcome>>,
    /// Last submitted search request whose background response should still be
    /// applied. Stale responses are ignored so quick successive searches cannot
    /// replace newer results.
    pub(crate) pending_search: Option<SearchRequest>,
    pub(crate) show_input_help: bool,
    pub(crate) status: Status,
    /// A completed slash-command response waiting for the renderer to decide
    /// whether it fits in the fixed-height entry box.
    pub(crate) pending_command_response: Option<String>,
    pub(crate) should_quit: bool,
    /// Sender for results of in-flight login/logout work spawned off the event
    /// loop. `None` until the main loop wires up the channel (and in unit tests).
    pub(crate) lifecycle_tx: Option<mpsc::UnboundedSender<LifecycleOutcome>>,
    /// True while a login or logout request is awaiting its result, so the UI
    /// stays responsive but a second lifecycle verb can't race the first.
    pub(crate) lifecycle_busy: bool,
    /// True while a `/send` upload is in flight, so its "uploading …" status
    /// line survives until the result lands rather than being clobbered by a
    /// second `/send` started mid-upload.
    pub(crate) media_send_busy: bool,
    /// True while an M19 room membership/moderation action is in flight, so
    /// progress and outcome status are not overwritten by unrelated refreshes.
    pub(crate) room_action_busy: bool,
    /// Sender for results of in-flight M19 room actions spawned off the event
    /// loop. `None` until the main loop wires up the channel (and in unit tests).
    pub(crate) room_action_tx: Option<mpsc::UnboundedSender<RoomActionOutcome>>,
    /// User-toggled hide for the accounts panel (independent of account count).
    pub(crate) accounts_panel_hidden: bool,
    /// User-toggled hide for the rooms panel.
    pub(crate) rooms_panel_hidden: bool,
    /// Path to the loaded config file, used by /saveconfig and /editconfig.
    pub(crate) config_path: PathBuf,
    /// Set by /editconfig; consumed by the main loop to suspend the TUI and open an editor.
    pub(crate) edit_config_requested: bool,
    /// Active room-list filter mode (ADR 0042). Replaces the old unread-only
    /// boolean; `RoomFilter::Unread` is the equivalent state.
    pub(crate) room_filter: RoomFilter,
    /// Active room-list sort mode for the unpinned section (ADR 0042).
    pub(crate) room_sort: RoomSort,
    /// Filter active before a live name-filter input began, restored if the user
    /// presses Esc to abandon it (ADR 0042). `None` outside name-filter input.
    pub(crate) room_filter_before_input: Option<RoomFilter>,
    /// Pinned rooms, ordered most recently pinned first (index 0 = top of the
    /// pinned section). Persisted to `[display] pinned_rooms` in the config file
    /// on every pin/unpin. See ADR 0038.
    pub(crate) pinned_rooms: Vec<RoomKey>,
    /// Display titles derived from members for rooms with no `m.room.name`/alias
    /// (e.g. DMs), so the room list shows the other participant's name instead of
    /// the raw room id. Filled lazily by background `/members` fetches.
    pub(crate) room_titles: HashMap<RoomKey, String>,
    /// In-flight and decoded images, account-scoped and bounded by LRU order.
    pub(crate) image_cache: HashMap<MediaKey, ImageState>,
    image_cache_order: VecDeque<MediaKey>,
    /// Sender end of the channel the main loop listens on for completed media
    /// work. `None` until `set_media_sender` is called (unit tests may
    /// omit it).
    pub(crate) media_tx: Option<mpsc::Sender<MediaResult>>,
    media_workers: Arc<Semaphore>,
    /// Terminal image protocol picker, detected before raw mode with halfblocks
    /// as the universal fallback.
    pub(crate) picker: Picker,
    /// Bounded cache of protocols encoded for a specific image and cell size.
    pub(crate) proto_cache: HashMap<ProtocolKey, ProtocolState>,
    proto_cache_order: VecDeque<ProtocolKey>,
    /// The active SAS verification flow, when `Mode::Verification` is open. The
    /// modal reads and mutates this; `None` whenever the modal is closed.
    pub(crate) verification: Option<VerificationFlow>,
    /// Changes periodically for Sixel inline thumbnails, forcing ratatui's diff
    /// renderer to retransmit pixels that terminal graphics layers may drop.
    pub(crate) sixel_inline_generation: u64,
    /// Changes periodically while a Sixel preview is open inside tmux, forcing
    /// ratatui's diff renderer to retransmit pixels that tmux does not retain.
    ///
    /// Per-preview, not global: [`App::open_popup`] resets it (and the deadline
    /// below) so every preview starts on the canonical variant and gets its
    /// first retransmit a full interval after *it* opened (#49).
    pub(crate) sixel_preview_generation: u64,
    /// When the open Sixel preview is next due a retransmit. Lives here rather
    /// than in the main loop because only `App` knows when a preview opens.
    pub(crate) sixel_preview_refresh_after: Instant,
    /// Encode requests that could not be started, for the debug overlay (#51).
    pub(crate) protocol_drops: ProtocolDropCounts,
    /// Set for one frame after the media-preview popup closes so `draw()` can
    /// issue a targeted Clear over the former popup area.  Sixel/iTerm2 pixels
    /// are not erased by ratatui's cell-diff pass when the image disappears, so
    /// without this explicit clear a ghost image lingers until something else
    /// overwrites those cells.
    pub(crate) clear_media_preview: bool,
    /// Set to `true` by `open_thread_panel` / `close_thread_panel`.  The main
    /// loop responds by emitting crossterm erase-line commands across every row
    /// of `last_messages_area` before the next draw.  A targeted
    /// `render_widget(Clear, area)` is insufficient when `messages_background`
    /// is `Color::Reset`: both the old and new buffer cells compare equal so
    /// ratatui's diff emits no terminal codes and sixel/halfblock pixels linger.
    pub(crate) force_terminal_clear: bool,
    /// Screen rects where pixel-protocol (sixel/iTerm2) image widgets were drawn
    /// on the previous frame.  These pixels are not part of ratatui's cell model,
    /// so when an image moves or disappears (scrolling, pane toggle, resize,
    /// preview close) the old pixels survive the cell-diff and linger as a
    /// misplaced "ghost" image.  `draw()` compares this against the current
    /// frame's image rects and force-repaints any cell an image just vacated.
    pub(crate) prev_image_rects: Vec<Rect>,
    /// The active thread root event id when the thread panel is open; `None` in
    /// the main timeline (ADR 0032 M2). The panel reuses the message pane —
    /// `selected_events()` returns the root plus its members while it is set.
    pub(crate) thread_panel: Option<String>,
    /// Cross-window reply context cache: replied-to events fetched via
    /// `GET …/events/{event_id}` when the target is older than the loaded slice
    /// (ADR 0032 M3). Keyed by `(account_id, event_id)`.
    pub(crate) reply_targets: HashMap<(Uuid, String), EventDto>,
    /// Server-aggregated thread summaries per room, fetched from
    /// `GET …/rooms/{room_id}/threads` (ADR 0032 M3). Keyed by thread root id.
    pub(crate) thread_summaries: HashMap<RoomKey, HashMap<String, crate::api::ThreadSummaryDto>>,
    /// Monotonic id for background relation refreshes. Results carry the id
    /// current when spawned so an older room load cannot overwrite newer caches.
    relation_refresh_next_id: u64,
    relation_refresh_latest: HashMap<RoomKey, u64>,
    /// Sender for completed background relation refreshes (thread summaries +
    /// cross-window reply targets). `None` until the main loop wires it up (and
    /// in unit tests), in which case relations stay in-slice-only.
    pub(crate) relations_tx: Option<mpsc::UnboundedSender<relations::RelationOutcome>>,
    /// Sender for completed background `/members` refreshes that resolve sender
    /// display names for live messages from unknown senders. `None` until the
    /// main loop wires it up (and in unit tests).
    pub(crate) members_tx: Option<mpsc::UnboundedSender<timeline::MembersOutcome>>,
    /// Earliest instant a room may trigger another background `/members` refresh,
    /// rate-limiting the live unknown-sender path (see `spawn_members_refresh`).
    members_refresh_after: HashMap<RoomKey, std::time::Instant>,
    /// The event the next sent message replies to (ADR 0032 M4), set by `/reply`
    /// or the reply hotkey. Mutually exclusive with `pending_thread`; the status
    /// line shows it while set, and `Escape` clears it.
    pub(crate) pending_reply: Option<String>,
    /// The thread root the next sent message joins (ADR 0032 M4), set by
    /// `/thread` on a message that has no thread yet. Mutually exclusive with
    /// `pending_reply`. While the thread panel is open, sends target its root
    /// even without this set.
    pub(crate) pending_thread: Option<String>,
    /// Live thread member event ids promoted to the main timeline. A thread
    /// member normally hides behind its root's badge; when a new reply arrives
    /// live and the thread panel is not open for that root, the reply is added
    /// here so `thread_visible` lets it through and the user sees it inline.
    /// Cleared per-root when the thread panel opens for that root.
    pub(crate) promoted_thread_events: std::collections::HashSet<String>,
    /// This install's device UUID (M12, ADR 0048), minted on first run and
    /// persisted next to the config file. Names this client in device-state
    /// PUTs; live frames carrying it back are our own echoes and are dropped.
    pub(crate) device_id: Uuid,
    /// Last known draft text per room (M12): the local mirror of the server's
    /// merged `drafts` device-state namespace, updated by our own flushes and
    /// by live frames from sibling devices.
    pub(crate) drafts: HashMap<RoomKey, String>,
    /// A compose-buffer change waiting out its debounce window before being
    /// PUT (M12). At most one — newer changes replace it.
    pub(crate) pending_draft_put: Option<drafts::PendingDraftPut>,
    /// The room whose draft the compose buffer currently mirrors (M12). Tracks
    /// the buffer across room switches and detours through other modes so a
    /// draft is never dropped or misattributed to the wrong room; `None` before
    /// the first room loads.
    pub(crate) compose_room: Option<RoomKey>,
    /// Sender for background draft-PUT failures. `None` until the main loop
    /// wires it up (and in unit tests, where flushes stay local-only).
    pub(crate) drafts_tx: Option<mpsc::UnboundedSender<DraftOutcome>>,
    /// Last known read marker per room (M12): the local mirror of the merged
    /// `read_markers` device-state namespace. Advances monotonically only.
    pub(crate) read_markers: HashMap<RoomKey, read_markers::ReadMarker>,
    /// Per-room read-receipt target (ADR 0089): the greatest `arrival_order`
    /// among the events this client displayed there. Advances monotonically on
    /// `arrival_order`, which is *not* the `read_markers` order — see
    /// [`read_markers`] for why these are two values, not one. Session-local: it
    /// is not device state and is deliberately never hydrated.
    pub(crate) receipt_targets: HashMap<RoomKey, read_markers::ReceiptTarget>,
    /// A read-marker advance waiting out its debounce window before being PUT
    /// (M12). One slot; arming for a different room flushes the old one.
    pub(crate) pending_marker_put: Option<read_markers::PendingMarkerPut>,
    /// The active outbound typing notice (ADR 0068 M19a), or `None` when not
    /// composing. One slot: only one room is typed in at a time.
    pub(crate) typing: Option<typing::TypingNotice>,
    /// Inbound typing + read-receipt overlays from other users (M18, ADR 0056).
    pub(crate) ephemeral: ephemeral::EphemeralState,
    /// TUI-local unread attention state for thread roots (ADR 0049). This is
    /// populated only from live events observed by this process and cleared when
    /// the user opens the corresponding thread panel.
    pub(crate) unread_threads: HashMap<RoomKey, HashMap<String, UnreadThread>>,
    /// Selected row in the unread-thread picker popup.
    pub(crate) unread_thread_selection: usize,
    /// Stable identity of the selected unread-thread row. The sorted picker
    /// list can reorder while live replies arrive, so Enter follows this
    /// identity rather than whatever item happens to occupy the old index.
    pub(crate) unread_thread_selected: Option<UnreadThreadSelection>,
}

#[derive(Default)]
pub(crate) struct RoomsState {
    pub(crate) rooms: Vec<RoomDto>,
    pub(crate) selected: Option<usize>,
    pub(crate) scroll: usize,
    pub(crate) page_size: usize,
    pub(crate) display_names: HashMap<RoomKey, HashMap<String, String>>,
    pub(crate) unread: HashMap<RoomKey, usize>,
}

pub(crate) struct AccountsState {
    /// Only Active accounts. Used for panel display, navigation, and filtering.
    pub(crate) accounts: Vec<AccountDto>,
    /// Every account returned by `GET /v1/accounts` (active + deactivated).
    /// Retained for lifecycle/status views without exposing logged-out accounts
    /// through active-only navigation.
    pub(crate) client_visible: Vec<AccountDto>,
    /// Account IDs known to be inactive (deactivated/deleting). Kept separately
    /// so room-list filtering can drop their rooms even though they are not
    /// displayed in the panel.
    pub(crate) inactive_ids: std::collections::HashSet<Uuid>,
    pub(crate) selected: AccountSelection,
    pub(crate) scroll: usize,
    pub(crate) page_size: usize,
}

impl Default for AccountsState {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            client_visible: Vec::new(),
            inactive_ids: std::collections::HashSet::new(),
            selected: AccountSelection::All,
            scroll: 0,
            page_size: 1,
        }
    }
}

pub(crate) struct MessagePane {
    pub(crate) events: HashMap<RoomKey, Vec<EventDto>>,
    pub(crate) selection: Option<String>,
    pub(crate) scroll: usize,
    pub(crate) page_size: usize,
    pub(crate) width: usize,
    pub(crate) line_ranges: Vec<std::ops::Range<usize>>,
    pub(crate) layout_event_ids: Vec<String>,
    /// Per-room opaque cursor for the next older page of history (`next_cursor`
    /// from the server). Absent when the room is at the beginning of history.
    pub(crate) history_cursors: HashMap<RoomKey, String>,
    /// True while a history-fetch request is in flight; prevents duplicate loads.
    pub(crate) loading_history: bool,
}

impl Default for MessagePane {
    fn default() -> Self {
        Self {
            events: HashMap::new(),
            selection: None,
            scroll: usize::MAX,
            page_size: 1,
            width: 80,
            line_ranges: Vec::new(),
            layout_event_ids: Vec::new(),
            history_cursors: HashMap::new(),
            loading_history: false,
        }
    }
}

#[derive(Default)]
pub(crate) struct InputState {
    pub(crate) buffer: String,
    pub(crate) cursor: usize,
    pub(crate) react_tab: Option<usize>,
    pub(crate) react_command_completion: Option<(String, usize)>,
    pub(crate) partial_room_completions: Option<Vec<String>>,
    pub(crate) room_command_completion: Option<(String, usize)>,
    pub(crate) logout_command_completion: Option<(String, usize)>,
    pub(crate) recover_command_completion: Option<(String, usize)>,
    pub(crate) delete_command_completion: Option<(String, usize)>,
    pub(crate) account_command_completion: Option<(String, usize)>,
    pub(crate) verify_command_completion: Option<(String, usize)>,
    pub(crate) filter_command_completion: Option<(String, usize)>,
    pub(crate) send_command_completion: Option<(String, usize)>,
}

#[derive(Default)]
pub(crate) struct LiveState {
    pub(crate) own_senders: HashMap<Uuid, String>,
    pub(crate) pending_own_event_id: Option<String>,
}

impl App {
    pub(crate) fn new(
        client: AxonClient,
        account_filter: Option<Uuid>,
        config: TuiConfig,
        picker: Picker,
    ) -> Self {
        let config_status = if config.created_default {
            format!("created default config at {}", config.path.display())
        } else {
            "connecting to Axon".to_owned()
        };
        let config_path = config.path.clone();
        let pinned_rooms = config
            .display
            .pinned_rooms
            .iter()
            .filter_map(|entry| RoomKey::parse_config_entry(entry))
            .collect();
        let room_filter = RoomFilter::parse(&config.display.room_filter);
        let room_sort = RoomSort::parse(&config.display.room_sort);
        Self {
            client,
            account_filter,
            shortcuts: config.shortcuts,
            colors: config.colors,
            display: config.display,
            rooms: RoomsState::default(),
            accounts: AccountsState::default(),
            messages: MessagePane::default(),
            input: InputState::default(),
            live: LiveState::default(),
            connection_state: ConnectionState::Unknown,
            mode: Mode::Compose,
            popup_scroll: 0,
            help_selection: 0,
            last_search: None,
            last_jump_ts: None,
            search_form: SearchFormState::default(),
            search_results: None,
            search_tx: None,
            pending_search: None,
            show_input_help: true,
            status: Status::Info(config_status),
            pending_command_response: None,
            should_quit: false,
            lifecycle_tx: None,
            lifecycle_busy: false,
            media_send_busy: false,
            room_action_busy: false,
            room_action_tx: None,
            image_cache: HashMap::new(),
            image_cache_order: VecDeque::new(),
            media_tx: None,
            media_workers: Arc::new(Semaphore::new(MEDIA_WORKERS)),
            picker,
            proto_cache: HashMap::new(),
            proto_cache_order: VecDeque::new(),
            verification: None,
            sixel_inline_generation: 0,
            sixel_preview_generation: 0,
            sixel_preview_refresh_after: Instant::now() + SIXEL_REFRESH_INTERVAL,
            protocol_drops: ProtocolDropCounts::default(),
            clear_media_preview: false,
            force_terminal_clear: false,
            prev_image_rects: Vec::new(),
            thread_panel: None,
            reply_targets: HashMap::new(),
            thread_summaries: HashMap::new(),
            relation_refresh_next_id: 0,
            relation_refresh_latest: HashMap::new(),
            relations_tx: None,
            members_tx: None,
            members_refresh_after: HashMap::new(),
            pending_reply: None,
            pending_thread: None,
            promoted_thread_events: std::collections::HashSet::new(),
            unread_threads: HashMap::new(),
            unread_thread_selection: 0,
            unread_thread_selected: None,
            accounts_panel_hidden: false,
            rooms_panel_hidden: false,
            config_path,
            edit_config_requested: false,
            room_filter,
            room_sort,
            room_filter_before_input: None,
            pinned_rooms,
            room_titles: HashMap::new(),
            device_id: Uuid::nil(),
            drafts: HashMap::new(),
            pending_draft_put: None,
            compose_room: None,
            drafts_tx: None,
            read_markers: HashMap::new(),
            receipt_targets: HashMap::new(),
            pending_marker_put: None,
            typing: None,
            ephemeral: ephemeral::EphemeralState::default(),
        }
    }

    /// Wire up the channel the main loop drains for spawned login/logout results.
    pub(crate) fn set_lifecycle_sender(&mut self, tx: mpsc::UnboundedSender<LifecycleOutcome>) {
        self.lifecycle_tx = Some(tx);
    }

    pub(crate) fn set_room_action_sender(&mut self, tx: mpsc::UnboundedSender<RoomActionOutcome>) {
        self.room_action_tx = Some(tx);
    }

    /// Wire up the channel the main loop drains for completed image downloads.
    pub(crate) fn set_media_sender(&mut self, tx: mpsc::Sender<MediaResult>) {
        self.media_tx = Some(tx);
    }

    /// Wire up the channel the main loop drains for completed search requests.
    pub(crate) fn set_search_sender(
        &mut self,
        tx: mpsc::UnboundedSender<search_flow::SearchOutcome>,
    ) {
        self.search_tx = Some(tx);
    }

    /// Wire up the channel the main loop drains for completed relation refreshes.
    pub(crate) fn set_relations_sender(
        &mut self,
        tx: mpsc::UnboundedSender<relations::RelationOutcome>,
    ) {
        self.relations_tx = Some(tx);
    }

    /// Wire up the channel the main loop drains for completed `/members` refreshes.
    pub(crate) fn set_members_sender(
        &mut self,
        tx: mpsc::UnboundedSender<timeline::MembersOutcome>,
    ) {
        self.members_tx = Some(tx);
    }

    /// The title to show for `room` in the room list. Named rooms use their
    /// `m.room.name`/canonical alias (via [`RoomDto::title`]); unnamed rooms (DMs)
    /// use a member-derived title once one has been fetched, falling back to the
    /// raw room id until then.
    pub(crate) fn room_list_title(&self, room: &crate::api::RoomDto) -> String {
        let named = room.name.as_deref().is_some_and(|n| !n.trim().is_empty())
            || room
                .canonical_alias
                .as_deref()
                .is_some_and(|a| !a.trim().is_empty());
        if named {
            return room.title().to_owned();
        }
        self.room_titles
            .get(&RoomKey::from(room))
            .cloned()
            .unwrap_or_else(|| room.title().to_owned())
    }

    /// Request a background download of `mxc_url` if it is not already cached
    /// or in flight. Does nothing if the image channel has not been wired up.
    pub(crate) fn request_image(&mut self, account_id: Uuid, mxc_url: String, is_encrypted: bool) {
        let key = MediaKey::new(account_id, mxc_url);
        if self.image_cache.contains_key(&key) {
            touch_lru(&mut self.image_cache_order, &key);
            return;
        }
        let Some(tx) = self.media_tx.clone() else {
            return;
        };
        if !evict_lru_where(
            &mut self.image_cache,
            &mut self.image_cache_order,
            IMAGE_CACHE_LIMIT,
            |state| !matches!(state, ImageState::Fetching),
        ) {
            return;
        }
        self.image_cache.insert(key.clone(), ImageState::Fetching);
        touch_lru(&mut self.image_cache_order, &key);
        media::spawn_image_fetch(
            self.client.clone(),
            key,
            is_encrypted,
            self.media_workers.clone(),
            tx,
        );
    }

    /// Ask for a terminal-protocol encoding of an already-decoded image.
    ///
    /// Returns why it did or did not start one. Both call sites are in `draw()`
    /// and ignore the answer — the value is that the outcomes are now named and
    /// testable, and that the two that are genuine faults are counted into
    /// [`App::protocol_drops`] for the `display.debug` overlay instead of
    /// vanishing. It cannot report through `self.status`: at 10 Hz from `draw`
    /// that would bulldoze whatever the user was reading (#51).
    pub(crate) fn request_protocol(&mut self, key: MediaKey, size: Size) -> ProtocolRequest {
        if size.width == 0 || size.height == 0 {
            return ProtocolRequest::EmptySize;
        }
        let protocol_key = ProtocolKey {
            media: key.clone(),
            size,
        };
        if self.proto_cache.contains_key(&protocol_key) {
            touch_lru(&mut self.proto_cache_order, &protocol_key);
            return ProtocolRequest::AlreadyPresent;
        }
        let Some(ImageState::Ready(image)) = self.image_cache.get(&key) else {
            // Expected: the fetch/decode has not landed yet and the next frame
            // asks again. Not a fault, so not counted.
            return ProtocolRequest::ImageNotReady;
        };
        let Some(tx) = self.media_tx.clone() else {
            // `set_media_sender` was never called. Every encode for the life of
            // the process is dropped here, which is exactly the kind of wiring
            // mistake that should not be silent.
            self.protocol_drops.channel_unwired += 1;
            return ProtocolRequest::ChannelUnwired;
        };
        let image = Arc::clone(image);
        if !evict_lru_where(
            &mut self.proto_cache,
            &mut self.proto_cache_order,
            PROTOCOL_CACHE_LIMIT,
            |state| !matches!(state, ProtocolState::Encoding),
        ) {
            // All PROTOCOL_CACHE_LIMIT slots are mid-encode, so there is nothing
            // evictable. Self-healing — a slot frees and the next frame retries
            // — but a sustained count here means thumbnails are starving.
            self.protocol_drops.cache_saturated += 1;
            return ProtocolRequest::CacheSaturated;
        }
        self.proto_cache
            .insert(protocol_key.clone(), ProtocolState::Encoding);
        touch_lru(&mut self.proto_cache_order, &protocol_key);
        media::spawn_protocol_encode(
            self.picker.clone(),
            image,
            protocol_key,
            self.media_workers.clone(),
            tx,
        );
        ProtocolRequest::Started
    }

    /// Install a completed fetch or encode. A result for an entry that is no
    /// longer awaiting one (evicted, or superseded) is discarded — the media
    /// workers' stale-result rule.
    pub(crate) fn handle_media_result(&mut self, result: MediaResult) {
        match result {
            MediaResult::Image { key, outcome } => {
                if !matches!(self.image_cache.get(&key), Some(ImageState::Fetching)) {
                    return;
                }
                let state = match outcome {
                    Ok(image) => ImageState::Ready(image),
                    Err(error) => ImageState::Failed(error),
                };
                self.image_cache.insert(key.clone(), state);
                touch_lru(&mut self.image_cache_order, &key);
            }
            MediaResult::Protocol { key, outcome } => {
                if !matches!(self.proto_cache.get(&key), Some(ProtocolState::Encoding)) {
                    return;
                }
                let state = match outcome {
                    Ok(protocol) => ProtocolState::Ready(protocol),
                    Err(error) => ProtocolState::Failed(error),
                };
                self.proto_cache.insert(key.clone(), state);
                touch_lru(&mut self.proto_cache_order, &key);
            }
        }
    }

    pub(crate) fn take_edit_config_request(&mut self) -> bool {
        std::mem::take(&mut self.edit_config_requested)
    }

    /// Called by the main loop after the editor process exits.
    pub(crate) fn apply_editor_result(&mut self, result: std::io::Result<()>) {
        match result {
            Ok(()) => self.reload_config(),
            Err(e) => self.status = Status::Info(format!("editor launch failed: {e}")),
        }
    }

    fn reload_config(&mut self) {
        let path = self.config_path.clone();
        match TuiConfig::load_or_create_at(path) {
            Ok(config) => {
                self.shortcuts = config.shortcuts;
                self.colors = config.colors;
                self.display = config.display;
                self.status = Status::Info("config reloaded".to_owned());
            }
            Err(e) => self.status = Status::Info(format!("config reload failed: {e}")),
        }
    }

    /// Returns true when the user is mid-command in a transient input mode
    /// or has an account lifecycle request in flight, where unsolicited status
    /// updates would overwrite an active prompt/progress message or disrupt
    /// in-progress text entry.
    pub(crate) fn is_mid_command(&self) -> bool {
        self.lifecycle_busy
            || self.media_send_busy
            || self.room_action_busy
            || matches!(
                self.mode,
                Mode::LoginUsername
                    | Mode::LoginPassword { .. }
                    | Mode::RecoveryKey { .. }
                    | Mode::ConfirmLogout { .. }
                    | Mode::ConfirmDelete { .. }
                    | Mode::ConfirmRoomAction { .. }
                    | Mode::Editing { .. }
                    | Mode::Reacting { .. }
                    | Mode::Unreacting { .. }
                    | Mode::Search(..)
                    | Mode::DateJump
                    | Mode::SearchForm
                    | Mode::SearchResults
                    | Mode::Verification
            )
    }

    pub(crate) fn dismiss_input_help(&mut self) {
        self.show_input_help = false;
    }

    /// Replace the account list. Only Active accounts go into `accounts.accounts`
    /// (for display and navigation); inactive IDs are recorded separately so
    /// `is_known_inactive_account` can still filter their rooms off the room list.
    pub(crate) fn set_accounts(&mut self, accounts: Vec<AccountDto>) {
        let selected_account_id = self.active_account_filter();
        self.accounts.inactive_ids = accounts
            .iter()
            .filter(|a| a.state != AccountState::Active)
            .map(|a| a.account_id)
            .collect();
        let active: Vec<AccountDto> = accounts
            .iter()
            .filter(|a| {
                a.state == AccountState::Active
                    && self
                        .account_filter
                        .is_none_or(|account_id| a.account_id == account_id)
            })
            .cloned()
            .collect();
        self.accounts.client_visible = accounts;
        self.accounts.accounts = active;
        self.accounts.selected = selected_account_id
            .and_then(|account_id| {
                self.accounts
                    .accounts
                    .iter()
                    .position(|account| account.account_id == account_id)
            })
            .map(AccountSelection::Account)
            .unwrap_or(AccountSelection::All);
    }

    pub(crate) fn accounts_panel_visible(&self) -> bool {
        !self.accounts_panel_hidden && self.accounts.accounts.len() >= 2
    }

    pub(crate) fn rooms_panel_visible(&self) -> bool {
        !self.rooms_panel_hidden
    }

    pub(crate) fn toggle_accounts_panel(&mut self) {
        self.accounts_panel_hidden = !self.accounts_panel_hidden;
        if self.accounts_panel_hidden
            && matches!(
                self.mode,
                Mode::AccountList | Mode::Search(SearchKind::Accounts, _)
            )
        {
            self.mode = Mode::Compose;
        }
    }

    pub(crate) fn toggle_rooms_panel(&mut self) {
        self.rooms_panel_hidden = !self.rooms_panel_hidden;
        if self.rooms_panel_hidden
            && matches!(
                self.mode,
                Mode::RoomList | Mode::Search(SearchKind::Rooms, _)
            )
        {
            self.mode = Mode::Compose;
        }
    }

    pub(crate) fn adjust_accounts_width(&mut self, delta: i16) {
        const MIN: u16 = 10;
        const MAX: u16 = 60;
        self.display.accounts_panel_width =
            (self.display.accounts_panel_width as i16 + delta).clamp(MIN as i16, MAX as i16) as u16;
    }

    pub(crate) fn adjust_rooms_width(&mut self, delta: i16) {
        self.display.rooms_panel_width_adj = self
            .display
            .rooms_panel_width_adj
            .saturating_add(delta)
            .clamp(-50, 50);
    }

    pub(crate) fn adjust_input_lines(&mut self, delta: i16) {
        self.display.input_lines = (self.display.input_lines as i16 + delta).clamp(1, 10) as u16;
    }

    pub(crate) fn active_account_filter(&self) -> Option<Uuid> {
        match self.accounts.selected {
            AccountSelection::All => None,
            AccountSelection::Account(idx) => self.accounts.accounts.get(idx).map(|a| a.account_id),
        }
    }

    /// `alt-u`: jump to the unread filter, or back to `All` if already on it.
    pub(crate) fn toggle_unread_filter(&mut self) {
        let next = if self.room_filter == RoomFilter::Unread {
            RoomFilter::All
        } else {
            RoomFilter::Unread
        };
        self.set_room_filter(next);
    }

    /// Set the room-list filter, persist it, and keep the selection visible.
    /// Surfaces the active filter in the status line so key-chord shortcuts give
    /// the same feedback as the `/filter` command.
    pub(crate) fn set_room_filter(&mut self, filter: RoomFilter) {
        self.room_filter = filter;
        self.sync_room_selection_to_account_filter();
        self.status = Status::from(format!("filter: {}", self.room_filter.label()));
        self.persist_room_view();
    }

    /// `alt-f`: advance to the next filter in the cycle.
    pub(crate) fn cycle_room_filter(&mut self) {
        let next = self.room_filter.next();
        self.set_room_filter(next);
    }

    /// Set the room-list sort mode, re-sort, persist, and surface it in the
    /// status line (so key-chord shortcuts give feedback too).
    pub(crate) fn set_room_sort(&mut self, sort: RoomSort) {
        self.room_sort = sort;
        self.resort_rooms();
        self.status = Status::from(format!("sort: {}", self.room_sort.label()));
        self.persist_room_view();
    }

    /// `alt-s`: advance to the next sort mode in the cycle.
    pub(crate) fn cycle_room_sort(&mut self) {
        let next = self.room_sort.next();
        self.set_room_sort(next);
    }

    /// Enter live name-filter input (ADR 0042): remember the current filter so
    /// Esc can restore it, start from an empty query, and switch to the input
    /// mode. Pre-seeds the query from an existing name filter.
    pub(crate) fn begin_room_name_filter(&mut self) {
        let seed = match &self.room_filter {
            RoomFilter::Name(q) => {
                self.room_filter_before_input = Some(RoomFilter::Name(q.clone()));
                q.clone()
            }
            other => {
                self.room_filter_before_input = Some(other.clone());
                String::new()
            }
        };
        self.update_room_name_filter(seed.clone());
        self.mode = Mode::Search(SearchKind::RoomNameFilter, seed);
    }

    /// Live-update the name filter as the user types. Does not persist (a name
    /// filter is session-only — it saves as `all`).
    pub(crate) fn update_room_name_filter(&mut self, query: String) {
        self.room_filter = RoomFilter::Name(query.to_lowercase());
        self.sync_room_selection_to_account_filter();
    }

    /// Abandon name-filter input: restore the pre-input filter (default `All`).
    pub(crate) fn cancel_room_name_filter(&mut self) {
        let restored = self
            .room_filter_before_input
            .take()
            .unwrap_or(RoomFilter::All);
        self.room_filter = restored;
        self.sync_room_selection_to_account_filter();
    }

    /// Persist the current sort + filter to `[display]`. Best-effort: a save
    /// failure surfaces in the status line but does not interrupt the UI.
    fn persist_room_view(&mut self) {
        if let Err(err) = TuiConfig::save_room_view(
            &self.config_path,
            self.room_sort.as_config_str(),
            self.room_filter.as_config_str(),
        ) {
            self.status = Status::from(format!("config save failed: {err}"));
        }
    }

    pub(crate) fn visible_room_indices(&self) -> Vec<usize> {
        let account = self.active_account_filter();
        let selected = self.rooms.selected;
        self.rooms
            .rooms
            .iter()
            .enumerate()
            .filter(|(_, r)| account.is_none_or(|id| r.account_id == id))
            .filter(|(i, r)| selected == Some(*i) || self.room_passes_filter(r))
            .map(|(i, _)| i)
            .collect()
    }

    /// Whether a room satisfies the active [`RoomFilter`]. The account filter and
    /// the "keep the selected room visible" rule are applied by the caller.
    fn room_passes_filter(&self, room: &RoomDto) -> bool {
        match &self.room_filter {
            RoomFilter::All => true,
            RoomFilter::Dms => rooms::is_likely_dm(room),
            RoomFilter::Groups => !rooms::is_likely_dm(room),
            RoomFilter::Unread => {
                self.rooms
                    .unread
                    .get(&RoomKey::from(room))
                    .copied()
                    .unwrap_or(0)
                    > 0
            }
            RoomFilter::Favorites => self.is_room_pinned(&RoomKey::from(room)),
            RoomFilter::Name(q) => {
                timeline::room_matches_search(room, q)
                    || self.room_list_title(room).to_ascii_lowercase().contains(q)
            }
        }
    }

    /// Clear every in-progress Tab-completion cycle. Any edit to the buffer
    /// invalidates whatever completion state was mid-cycle, so every mutating
    /// entry point (character insert/paste, backspace, delete) calls this
    /// first — kept as one method (rather than the field list inlined at each
    /// call site) so a new completion field only needs updating here.
    pub(crate) fn reset_completion_state(&mut self) {
        self.input.react_command_completion = None;
        self.input.partial_room_completions = None;
        self.input.room_command_completion = None;
        self.input.logout_command_completion = None;
        self.input.recover_command_completion = None;
        self.input.delete_command_completion = None;
        self.input.account_command_completion = None;
        self.input.verify_command_completion = None;
        self.input.filter_command_completion = None;
        self.input.send_command_completion = None;
    }

    pub(crate) fn insert_char(&mut self, ch: char) {
        self.reset_completion_state();
        self.input.buffer.insert(self.input.cursor, ch);
        self.input.cursor += ch.len_utf8();
    }

    /// Bulk-insert a whole string at the cursor in one edit — used for
    /// bracketed paste (drag-and-drop, clipboard paste) so a large paste is
    /// one atomic buffer change rather than a loop of `insert_char` calls,
    /// which would otherwise re-run completion-state invalidation and
    /// draft-debounce bookkeeping once per pasted character.
    pub(crate) fn insert_str(&mut self, text: &str) {
        self.reset_completion_state();
        self.input.buffer.insert_str(self.input.cursor, text);
        self.input.cursor += text.len();
    }

    /// Handle a bracketed-paste block (a terminal drag-and-drop drop, or a
    /// clipboard paste) delivered as one atomic string from the input thread.
    /// Only applied in modes that route ordinary character keys into
    /// `self.input.buffer` (the same modes `insert_char`'s callers gate on in
    /// `keymap.rs`); other modes either don't accept free text (list
    /// navigation, popups) or use a different buffer entirely (`SearchForm`'s
    /// per-field buffer), so a paste there is a no-op rather than corrupting
    /// unrelated state.
    pub(crate) fn handle_paste(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        let accepts_free_text = matches!(
            self.mode,
            Mode::Compose
                | Mode::LoginUsername
                | Mode::LoginPassword { .. }
                | Mode::RecoveryKey { .. }
                | Mode::ConfirmDelete { .. }
                | Mode::Editing { .. }
                | Mode::Reacting { .. }
                | Mode::DateJump
        );
        if !accepts_free_text {
            return;
        }
        self.dismiss_input_help();
        self.insert_str(&text);
    }

    pub(crate) fn backspace(&mut self) {
        self.reset_completion_state();
        if self.input.cursor == 0 {
            return;
        }
        let previous = self.input.buffer[..self.input.cursor]
            .char_indices()
            .last()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.input
            .buffer
            .replace_range(previous..self.input.cursor, "");
        self.input.cursor = previous;
    }

    pub(crate) fn delete_forward(&mut self) {
        self.reset_completion_state();
        if self.input.cursor >= self.input.buffer.len() {
            return;
        }
        let next = self.input.buffer[self.input.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.input.cursor + i)
            .unwrap_or(self.input.buffer.len());
        self.input.buffer.replace_range(self.input.cursor..next, "");
    }

    pub(crate) fn move_cursor_to_start(&mut self) {
        self.input.cursor = 0;
    }

    pub(crate) fn move_cursor_to_end(&mut self) {
        self.input.cursor = self.input.buffer.len();
    }

    pub(crate) fn move_cursor_left(&mut self) {
        if self.input.cursor == 0 {
            return;
        }
        self.input.cursor = self.input.buffer[..self.input.cursor]
            .char_indices()
            .last()
            .map(|(index, _)| index)
            .unwrap_or(0);
    }

    pub(crate) fn move_cursor_right(&mut self) {
        if self.input.cursor >= self.input.buffer.len() {
            return;
        }
        self.input.cursor += self.input.buffer[self.input.cursor..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(0);
    }

    pub(crate) fn delete_word_back(&mut self) {
        self.reset_completion_state();
        if self.input.cursor == 0 {
            return;
        }
        let s = &self.input.buffer[..self.input.cursor];
        let chars: Vec<(usize, char)> = s.char_indices().collect();
        let mut i = chars.len();
        while i > 0 && chars[i - 1].1.is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].1.is_whitespace() {
            i -= 1;
        }
        let new_cursor = chars.get(i).map(|(idx, _)| *idx).unwrap_or(0);
        self.input
            .buffer
            .replace_range(new_cursor..self.input.cursor, "");
        self.input.cursor = new_cursor;
    }

    pub(crate) fn move_cursor_word_left(&mut self) {
        let s = &self.input.buffer[..self.input.cursor];
        let chars: Vec<(usize, char)> = s.char_indices().collect();
        let mut i = chars.len();
        while i > 0 && chars[i - 1].1.is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].1.is_whitespace() {
            i -= 1;
        }
        self.input.cursor = chars.get(i).map(|(idx, _)| *idx).unwrap_or(0);
    }

    pub(crate) fn move_cursor_word_right(&mut self) {
        let s = &self.input.buffer[self.input.cursor..];
        let chars: Vec<(usize, char)> = s.char_indices().collect();
        let mut i = 0;
        while i < chars.len() && !chars[i].1.is_whitespace() {
            i += 1;
        }
        while i < chars.len() && chars[i].1.is_whitespace() {
            i += 1;
        }
        let advance = chars.get(i).map(|(idx, _)| *idx).unwrap_or(s.len());
        self.input.cursor += advance;
    }

    pub(crate) async fn handle_command(&mut self, command: Command) {
        let tracks_response =
            !matches!(&command, Command::Send(_) | Command::Empty | Command::Quit);
        self.pending_command_response = None;
        self.handle_command_inner(command).await;
        if tracks_response {
            self.queue_completed_command_response();
        }
    }

    pub(crate) fn queue_completed_command_response(&mut self) {
        if self.mode != Mode::Compose || self.is_mid_command() {
            return;
        }
        let response = self.status.text(self.display.debug);
        if !response.is_empty() {
            self.pending_command_response = Some(response);
        }
    }

    async fn handle_command_inner(&mut self, command: Command) {
        match command {
            Command::Login {
                username,
                password,
                homeserver,
            } => {
                self.start_login(username, password, homeserver).await;
            }
            Command::Logout(target) => self.start_logout(target),
            Command::Recover(target) => self.start_recover(target),
            Command::Delete(target) => self.start_delete(target),
            Command::Room(target) => self.switch_room(&target).await,
            Command::Pin(target) => self.pin_room(target.as_deref()),
            Command::Unpin(target) => self.unpin_room(target.as_deref()),
            Command::Filter(arg) => self.set_room_filter(RoomFilter::parse(&arg)),
            Command::Sort(arg) => self.set_room_sort(RoomSort::parse(&arg)),
            Command::Account(target) => {
                if self.switch_account(&target) {
                    self.load_selected_timeline().await;
                }
            }
            Command::Status => self.open_popup(PopupKind::Status),
            Command::Event(event_id) => self.show_event(&event_id).await,
            Command::Whoami => self.show_whoami(),
            Command::Whereami => self.show_whereami(),
            Command::Search(input) => self.open_or_run_search(input).await,
            Command::React(None) => {
                self.select_most_recent_message_if_needed();
                self.start_react_to_selected_message();
            }
            Command::React(Some(input)) => {
                let (event_id, reaction_key) = match self.prepare_reaction(&input) {
                    Ok(reaction) => reaction,
                    Err(message) => {
                        self.status = Status::from(message);
                        return;
                    }
                };
                self.send_react(&event_id, &reaction_key).await;
            }
            Command::Unreact => {
                self.select_most_recent_message_if_needed();
                self.start_unreact_from_selected_message().await;
            }
            Command::Reply => {
                self.select_most_recent_message_if_needed();
                self.start_reply_to_selected_message();
            }
            Command::Thread => {
                self.select_most_recent_message_if_needed();
                self.start_thread_from_selected_message().await;
            }
            Command::UnreadThreads => self.open_unread_threads_picker(),
            Command::Leave => self.start_leave_room(),
            Command::Forget(target) => self.start_forget_room(target.as_deref()),
            Command::Invite(user_id) => self.start_invite_user(&user_id),
            Command::Kick(input) => self.start_kick_user(input),
            Command::Ban(input) => self.start_ban_user(input),
            Command::Unban(input) => self.start_unban_user(input),
            Command::Verify(device_id) => self.start_verification(device_id),
            Command::Bundle(event_id) => self.show_verification_bundle(&event_id).await,
            Command::Help => self.open_popup(PopupKind::Help),
            Command::Shortcuts => self.open_popup(PopupKind::Shortcuts),
            Command::Refresh => {
                self.refresh_rooms().await;
            }
            Command::EditConfig => {
                self.edit_config_requested = true;
            }
            Command::SaveConfig => {
                match TuiConfig::save_display(
                    &self.config_path,
                    self.display.input_lines,
                    self.display.accounts_panel_width,
                    self.display.rooms_panel_width_adj,
                ) {
                    Ok(()) => {
                        self.status =
                            Status::Info(format!("config saved to {}", self.config_path.display()))
                    }
                    Err(e) => self.status = Status::Info(format!("save failed: {e}")),
                }
            }
            Command::Quit => self.should_quit = true,
            Command::Send(body) => {
                let formatted = markdown_to_html_if_detected(&body)
                    .map(|html| ("org.matrix.custom.html".to_owned(), html));
                self.send_message_to_room(&body, formatted);
            }
            Command::SendHtml(html) => {
                let plain = strip_html_to_plain(&html);
                let plain = if plain.is_empty() {
                    html.clone()
                } else {
                    plain
                };
                self.send_message_to_room(
                    &plain,
                    Some(("org.matrix.custom.html".to_owned(), html)),
                );
            }
            Command::SendLiteral(body) => self.send_message_to_room(&body, None),
            Command::SendMedia { path, caption } => self.send_media_to_room(path, caption),
            Command::Rainbow(text) => {
                let html = rainbow_html(&text);
                self.send_message_to_room(&text, Some(("org.matrix.custom.html".to_owned(), html)));
            }
            Command::Spoiler { reason, text } => {
                let (html, plain) = spoiler_html(reason.as_deref(), &text);
                self.send_message_to_room(
                    &plain,
                    Some(("org.matrix.custom.html".to_owned(), html)),
                );
            }
            Command::JumpToDate(ts) => {
                self.jump_to_date(ts).await;
            }
            Command::JumpToTop => {
                self.jump_to_top().await;
            }
            Command::Invalid(message)
            | Command::ApiUnsupported(message)
            | Command::Unknown(message) => {
                self.status = Status::Info(message);
            }
            Command::Empty => {}
        }
    }

    fn open_popup(&mut self, kind: PopupKind) {
        self.popup_scroll = 0;
        if kind == PopupKind::Help {
            self.help_selection = 0;
        }
        if kind == PopupKind::MediaPreview {
            // Start every preview from the canonical encoding and give it a
            // full interval before its first retransmit. Both used to carry
            // over from the previous preview — the counter is global and the
            // deadline was a main-loop local — so a fresh preview could open on
            // the alternate variant and be retransmitted immediately, or wait
            // out most of an interval that had elapsed while nothing was open
            // (#49).
            self.sixel_preview_generation = 0;
            self.sixel_preview_refresh_after = Instant::now() + SIXEL_REFRESH_INTERVAL;
        }
        self.mode = Mode::Popup(kind);
    }

    /// Fetch and display the per-event verification bundle (M7c / ADR 0031). The
    /// pretty-printed JSON is surfaced through the standard command-response path,
    /// which promotes it to a scrollable popup when it overflows the entry box.
    pub(crate) async fn show_verification_bundle(&mut self, event_id: &str) {
        let Some(room) = self.selected_room() else {
            self.status = Status::from("select a room before using /bundle".to_owned());
            return;
        };
        let account_id = room.account_id;
        match self
            .client
            .get_verification_bundle(account_id, event_id)
            .await
        {
            Ok(bundle) => {
                let text =
                    serde_json::to_string_pretty(&bundle).unwrap_or_else(|_| bundle.to_string());
                self.status = Status::Info(format!("verification bundle for {event_id}:\n{text}"));
            }
            Err(err) => self.status = Status::Info(format!("bundle read failed: {err}")),
        }
    }

    pub(crate) fn open_selected_media_preview(&mut self) {
        let Some(event) = self.selected_message_event() else {
            self.status = Status::Info("select an image message first".to_owned());
            return;
        };
        let Some((account_id, mxc_url)) = event.image_mxc() else {
            self.status = Status::Info("selected message has no image".to_owned());
            return;
        };
        let encrypted = event.image_is_encrypted();
        self.request_image(account_id, mxc_url, encrypted);
        self.open_popup(PopupKind::MediaPreview);
    }

    fn show_whereami(&mut self) {
        if self.selected_room().is_none() {
            self.status = Status::Info("select a room before using /whereami".to_owned());
            return;
        }
        self.open_popup(PopupKind::RoomInfo);
    }

    fn show_whoami(&mut self) {
        let Some(room) = self.selected_room() else {
            self.status = Status::Info("select a room before using /whoami".to_owned());
            return;
        };
        let Some(user_id) = room.account_user_id.as_deref() else {
            self.status = Status::Info("current user is unavailable for this room".to_owned());
            return;
        };

        let key = RoomKey::from(room);
        let display_name = self
            .rooms
            .display_names
            .get(&key)
            .and_then(|names| names.get(user_id))
            .filter(|name| !name.trim().is_empty())
            .map(String::as_str)
            .unwrap_or("unknown");
        self.status = Status::Info(format!(
            "Matrix ID: {user_id}; Display Name: {display_name}"
        ));
    }

    pub(crate) fn start_reply_to_selected_message(&mut self) {
        let Some(event) = self.selected_message_event() else {
            self.status = Status::from("select a displayed message before replying".to_owned());
            return;
        };
        if event.event_id.starts_with("local-echo:") {
            self.status = Status::from(PENDING_ECHO_MSG.to_owned());
            return;
        }
        let event_id = event.event_id.clone();
        // Reply and thread compose targets are mutually exclusive (ADR 0032 M4).
        self.pending_thread = None;
        self.pending_reply = Some(event_id.clone());
        self.mode = Mode::Compose;
        self.status = Status::EventAction {
            debug: format!("replying to {event_id} - Esc to cancel"),
            redacted: "replying to message - Esc to cancel",
        };
    }

    fn select_most_recent_message_if_needed(&mut self) {
        if self.selected_message_event().is_some() {
            return;
        }
        self.messages.selection = self
            .selected_events()
            .last()
            .map(|event| event.event_id.clone());
    }

    fn prepare_reaction(&mut self, input: &str) -> Result<(String, String), String> {
        self.select_most_recent_message_if_needed();
        let event_id = self
            .selected_message_id()
            .map(str::to_owned)
            .ok_or_else(|| "no displayed messages".to_owned())?;
        if event_id.starts_with("local-echo:") {
            return Err(PENDING_ECHO_MSG.to_owned());
        }
        let reaction_key = self
            .take_reaction_key(input)
            .ok_or_else(|| format!("unknown or ambiguous emoji: {input}"))?;
        Ok((event_id, reaction_key))
    }

    /// Open the thread panel for the selected message (ADR 0032 M2/M3). When the
    /// message is itself a thread member or a thread root, the panel opens at its
    /// root. Starting a *new* thread on a standalone message needs the send path
    /// (M4), which is not yet wired, so that case reports the gap.
    pub(crate) async fn start_thread_from_selected_message(&mut self) {
        let Some(event) = self.selected_message_event() else {
            self.status =
                Status::from("select a displayed message before opening a thread".to_owned());
            return;
        };
        if event.event_id.starts_with("local-echo:") {
            self.status = Status::from(PENDING_ECHO_MSG.to_owned());
            return;
        }
        let account_id = event.account_id;
        let event_id = event.event_id.clone();
        let root = if let Some(root) = event.thread_relation() {
            root.to_owned()
        } else if self.is_thread_root(&event_id) {
            event_id
        } else {
            // No thread exists here yet — compose a new one rooted at this message
            // (ADR 0032 M4). The next sent message becomes its first reply.
            self.pending_reply = None;
            self.pending_thread = Some(event_id.clone());
            self.mode = Mode::Compose;
            self.status = Status::EventAction {
                debug: format!("replying in new thread on {event_id} - Esc to cancel"),
                redacted: "replying in new thread - Esc to cancel",
            };
            return;
        };
        self.open_thread_panel(account_id, root).await;
    }

    pub(crate) fn start_edit_selected_message(&mut self) {
        let Some(event) = self.selected_message_event() else {
            self.status = Status::from("select a displayed message before editing".to_owned());
            return;
        };
        if event.event_id.starts_with("local-echo:") {
            self.status = Status::from(PENDING_ECHO_MSG.to_owned());
            return;
        }
        let event_id = event.event_id.clone();
        let body = event.display_body();
        // Settle the room's draft before the buffer is repurposed for the edit,
        // so returning to compose restores it instead of tombstoning it (M12).
        self.flush_pending_draft_now();
        self.input.buffer = body;
        self.move_cursor_to_end();
        self.mode = Mode::Editing {
            event_id: event_id.clone(),
        };
        self.status = Status::EventAction {
            debug: format!("editing {} - Esc to cancel", event_id),
            redacted: "editing message - Esc to cancel",
        };
    }

    fn send_message_to_room(&mut self, body: &str, formatted: Option<(String, String)>) {
        let Some(room) = self.selected_room().cloned() else {
            self.status = Status::from("select a room before sending".to_owned());
            return;
        };
        let Some(tx) = self.lifecycle_tx.clone() else {
            return;
        };
        // Consume any pending reply/thread target (ADR 0032 M4): the relation is
        // attached to exactly this send, then compose returns to normal. While the
        // thread panel is open, a send defaults to that thread even without an
        // explicit `/thread`.
        let reply_to = self.pending_reply.take();
        let thread_root = self
            .pending_thread
            .take()
            .or_else(|| self.thread_panel.clone());
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let sender = room
            .account_user_id
            .clone()
            .or_else(|| self.live.own_senders.get(&room.account_id).cloned())
            .or_else(|| {
                // Fallback: account_user_id is always present on AccountDto even
                // when the room DTO omits it (e.g. the server hasn't joined yet).
                self.accounts
                    .accounts
                    .iter()
                    .find(|a| a.account_id == room.account_id)
                    .map(|a| a.user_id.clone())
            })
            .unwrap_or_default();
        // Optimistic echo: insert before spawning so the message appears on the
        // very next frame. The spawned task delivers the real event_id (or error)
        // back via the lifecycle channel without blocking the render loop.
        let temp_id = format!("local-echo:{now_ms}");
        let key = RoomKey {
            account_id: room.account_id,
            room_id: room.room_id.clone(),
        };
        let echo_content = formatted.as_ref().map(|(fmt, fb)| {
            serde_json::json!({
                "msgtype": "m.text",
                "body": body,
                "format": fmt,
                "formatted_body": fb,
            })
        });
        // Mirror the relation the server will build so the optimistic echo gets the
        // same reply/thread treatment as the real event (reply context line; thread
        // member shown in the panel and counted on the root's badge).
        let echo_relates_to = match (&thread_root, &reply_to) {
            (Some(root), reply) => Some(serde_json::json!({
                "rel_type": "m.thread",
                "event_id": root,
                "m.in_reply_to": {
                    "event_id": reply.clone().unwrap_or_else(|| root.clone()),
                    "is_falling_back": reply.is_none(),
                },
            })),
            (None, Some(reply)) => Some(serde_json::json!({
                "m.in_reply_to": { "event_id": reply },
            })),
            (None, None) => None,
        };
        let echo = EventDto {
            account_id: room.account_id,
            event_id: temp_id.clone(),
            room_id: room.room_id.clone(),
            sender,
            state_key: None,
            // A local echo has not been ingested, so it has no arrival position
            // — the real one arrives with the confirming event. `i64::MIN` can
            // never win the max-by-`arrival_order` receipt selection, so an echo
            // can never be receipted (ADR 0089).
            arrival_order: i64::MIN,
            origin_ts: now_ms,
            event_type: "m.room.message".to_owned(),
            content: echo_content,
            body: Some(body.to_owned()),
            relates_to: echo_relates_to,
            redacted: false,
            redaction_event_id: None,
            reactions: None,
            sender_trust: None,
        };
        self.messages.scroll = usize::MAX;
        // Clear selection so the next message-targeted command auto-selects
        // rather than staying on whatever message was selected before the send.
        self.messages.selection = None;
        self.messages
            .events
            .entry(key.clone())
            .or_default()
            .push(echo);

        let client = self.client.clone();
        let body = body.to_owned();
        tokio::spawn(async move {
            let fmt_refs = formatted
                .as_ref()
                .map(|(fmt, fb)| (fmt.as_str(), fb.as_str()));
            let relation = SendRelation {
                reply_to: reply_to.as_deref(),
                thread_root: thread_root.as_deref(),
            };
            let result = client
                .send_message(room.account_id, &room.room_id, &body, fmt_refs, relation)
                .await
                .map(|r| r.event_id)
                .map_err(|e| e.to_string());
            let _ = tx.send(LifecycleOutcome::MessageSent {
                key,
                temp_id,
                result,
            });
        });
    }

    /// `/send <path> [caption]` (ADR 0059/0062): stage `path`'s bytes then
    /// send them into the current room. Runs entirely off the event loop
    /// (root AGENTS.md "never `await` an API call from key handling") and
    /// reports back through the same `lifecycle_tx`/`LifecycleOutcome`
    /// channel `send_message_to_room` uses, so the busy status line survives
    /// until the result lands. No optimistic local echo — the sent event
    /// arrives over `/v1/ws` like any other mutation.
    fn send_media_to_room(&mut self, path: String, caption: Option<String>) {
        let Some(room) = self.selected_room().cloned() else {
            self.status = Status::from("select a room before sending".to_owned());
            return;
        };
        let Some(tx) = self.lifecycle_tx.clone() else {
            return;
        };
        if self.media_send_busy {
            self.status = Status::Info("a /send upload is already in progress".to_owned());
            return;
        }
        // Consume any pending reply/thread target the same way a plain send
        // does (ADR 0032 M4), so /reply and /thread compose identically for
        // media.
        let reply_to = self.pending_reply.take();
        let thread_root = self
            .pending_thread
            .take()
            .or_else(|| self.thread_panel.clone());
        let key = RoomKey {
            account_id: room.account_id,
            room_id: room.room_id.clone(),
        };
        let fs_path = expand_send_path(&path);
        let filename = fs_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        self.media_send_busy = true;
        self.status = Status::Info(format!("uploading {filename}…"));

        let client = self.client.clone();
        tokio::spawn(async move {
            let result = async {
                let size = tokio::fs::metadata(&fs_path)
                    .await
                    .map_err(|err| format!("read failed: {err}"))?
                    .len();
                if size > MAX_UPLOAD_BYTES {
                    return Err(format!(
                        "{filename} is {} MiB, over the {} MiB /send limit",
                        size / (1024 * 1024),
                        MAX_UPLOAD_BYTES / (1024 * 1024)
                    ));
                }
                let bytes = tokio::fs::read(&fs_path)
                    .await
                    .map_err(|err| format!("read failed: {err}"))?;
                let (kind, content_type) = media_kind_and_content_type(&filename);
                let staged = client
                    .stage_upload(room.account_id, kind, &filename, Some(content_type), bytes)
                    .await
                    .map_err(|err| err.to_string())?;
                let relation = SendRelation {
                    reply_to: reply_to.as_deref(),
                    thread_root: thread_root.as_deref(),
                };
                client
                    .send_media(
                        room.account_id,
                        &room.room_id,
                        staged.upload_id,
                        caption.as_deref(),
                        relation,
                    )
                    .await
                    .map(|r| r.event_id)
                    .map_err(|err| err.to_string())
            }
            .await;
            let _ = tx.send(LifecycleOutcome::MediaSent { key, result });
        });
    }

    pub(crate) async fn send_edit(&mut self, event_id: &str, body: &str) {
        let Some(room) = self.selected_room().cloned() else {
            self.status = Status::from("no room selected".to_owned());
            return;
        };
        match self
            .client
            .edit_message(room.account_id, &room.room_id, event_id, body, None)
            .await
        {
            Ok(result) => {
                let key = RoomKey::from(&room);
                if let Some(events) = self.messages.events.get_mut(&key) {
                    if let Some(e) = events.iter_mut().find(|e| e.event_id == event_id) {
                        e.body = Some(body.to_owned());
                    }
                }
                self.status = Status::EventAction {
                    debug: format!("edited: {}", result.event_id),
                    redacted: "edited",
                };
            }
            Err(err) => self.status = Status::Info(format!("edit failed: {err}")),
        }
    }

    pub(crate) async fn redact_selected_message(&mut self) {
        let Some(event) = self.selected_message_event() else {
            self.status = Status::from("select a displayed message before redacting".to_owned());
            return;
        };
        if event.event_id.starts_with("local-echo:") {
            self.status = Status::from(PENDING_ECHO_MSG.to_owned());
            return;
        }
        let event_id = event.event_id.clone();
        let room = self.selected_room().cloned().expect("event implies room");
        match self
            .client
            .redact_event(room.account_id, &room.room_id, &event_id, None)
            .await
        {
            Ok(result) => {
                let key = RoomKey::from(&room);
                if let Some(events) = self.messages.events.get_mut(&key) {
                    if let Some(e) = events.iter_mut().find(|e| e.event_id == event_id) {
                        e.redacted = true;
                    }
                }
                self.status = Status::EventAction {
                    debug: format!("redacted: {}", result.event_id),
                    redacted: "redacted",
                };
            }
            Err(err) => self.status = Status::Info(format!("redact failed: {err}")),
        }
    }
}

pub(crate) fn relative_room_index(current: usize, len: usize, offset: isize) -> usize {
    if len == 0 {
        return 0;
    }
    if offset.is_negative() {
        current
            .checked_sub(offset.unsigned_abs())
            .unwrap_or(len.saturating_sub(1))
    } else {
        (current + offset as usize) % len
    }
}

pub(crate) fn cycle_index(current: usize, len: usize, reverse: bool) -> usize {
    if reverse {
        (current + len - 1) % len
    } else {
        (current + 1) % len
    }
}

pub(crate) fn selected_message_target_index(
    events: &[&EventDto],
    selected_message: Option<&str>,
    offset: isize,
) -> usize {
    if events.is_empty() {
        return 0;
    }
    let Some(current) = selected_message
        .and_then(|event_id| events.iter().position(|event| event.event_id == event_id))
    else {
        return if offset.is_negative() {
            events.len().saturating_sub(1)
        } else {
            0
        };
    };
    if offset.is_negative() {
        current.saturating_sub(offset.unsigned_abs())
    } else {
        current
            .saturating_add(offset as usize)
            .min(events.len().saturating_sub(1))
    }
}

/// Returns the next/previous value from `matches` (a sorted list of source-list indices)
/// relative to `current`, with optional wrap-around.
pub(crate) fn next_match_index(
    matches: &[usize],
    current: Option<usize>,
    forward: bool,
    wrap: bool,
) -> Option<usize> {
    if forward {
        let after = current.map(|i| i + 1).unwrap_or(0);
        let found = matches.iter().copied().find(|&i| i >= after);
        if found.is_some() || !wrap {
            found
        } else {
            matches.first().copied()
        }
    } else {
        let before = current.unwrap_or(0);
        let found = matches.iter().copied().rev().find(|&i| i < before);
        if found.is_some() || !wrap {
            found
        } else {
            matches.last().copied()
        }
    }
}

pub(crate) fn match_status(match_num: usize, total: usize) -> Status {
    Status::from(format!("match {} of {}", match_num, total))
}

fn touch_lru<K: Clone + Eq>(order: &mut VecDeque<K>, key: &K) {
    if let Some(index) = order.iter().position(|candidate| candidate == key) {
        order.remove(index);
    }
    order.push_back(key.clone());
}

fn evict_lru_where<K, V>(
    cache: &mut HashMap<K, V>,
    order: &mut VecDeque<K>,
    limit: usize,
    can_evict: impl Fn(&V) -> bool,
) -> bool
where
    K: Clone + Eq + std::hash::Hash,
{
    if cache.len() < limit {
        return true;
    }
    let Some(index) = order
        .iter()
        .position(|key| cache.get(key).is_some_and(&can_evict))
    else {
        return false;
    };
    let Some(oldest) = order.remove(index) else {
        return false;
    };
    cache.remove(&oldest);
    true
}

/// Apply the EXIF orientation tag to `img` so it displays upright, matching
/// what other Matrix clients show. `load_from_memory` decodes raw pixels but
/// ignores EXIF, so without this correction rotated JPEGs appear sideways.
/// Returns `img` unchanged if EXIF is absent, unreadable, or already upright.
pub(super) fn apply_exif_orientation(
    img: image::DynamicImage,
    bytes: &[u8],
) -> image::DynamicImage {
    use std::io::Cursor;
    let orientation = exif::Reader::new()
        .read_from_container(&mut Cursor::new(bytes))
        .ok()
        .and_then(|exif| {
            exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
                .and_then(|f| f.value.get_uint(0))
        })
        .unwrap_or(1);

    // EXIF orientation values 1–8; 1 = already upright.
    match orientation {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

/// Identify an image format from raw magic bytes without relying on the
/// compiled-in feature set of the `image` crate. Returns a short description
/// including a hex dump of the first bytes for truly unrecognized content.
pub(super) fn sniff_format(bytes: &[u8]) -> String {
    if bytes.starts_with(b"\xFF\xD8\xFF") {
        return "JPEG".into();
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1A\n") {
        return "PNG".into();
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return "GIF".into();
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return "WebP".into();
    }
    if bytes.starts_with(b"BM") {
        return "BMP".into();
    }
    if bytes.starts_with(b"II\x2A\x00") || bytes.starts_with(b"MM\x00\x2A") {
        return "TIFF".into();
    }
    // ISO Base Media File Format container: AVIF, HEIC, HEIF, MP4, …
    if bytes.get(4..8) == Some(b"ftyp") {
        return match bytes.get(8..12) {
            Some(b"avif") | Some(b"avis") => "AVIF".into(),
            Some(b"heic") | Some(b"heis") | Some(b"heim") | Some(b"heix") => "HEIC".into(),
            Some(b"mif1") | Some(b"msf1") => "HEIF".into(),
            _ => "ISO BMFF (AVIF/HEIC/MP4/…)".into(),
        };
    }
    if bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml") || bytes.starts_with(b"<SVG") {
        return "SVG (not supported)".into();
    }
    if bytes.starts_with(b"\x00\x00\x01\x00") {
        return "ICO".into();
    }
    // Not a recognized image format — could be a JSON/HTML error body served
    // with a 2xx status. Show the first bytes so the cause is obvious.
    let prefix: String = bytes
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    let printable: String = bytes
        .iter()
        .take(16)
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        })
        .collect();
    format!("unknown — first bytes: {prefix}  ({printable})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{
        AccountDto, AccountState, EmojiDto, MemberDto, TimelinePage, VerificationFrame,
        VerificationFrameDto,
    };
    use crate::app::search_flow::{SearchJumpAction, SearchJumpThreadLoad, SearchOutcome};
    use crate::command::HELP_COMMANDS;
    use crate::config::TimeFormat;
    use crate::ui::{entry_status_text, popup_shortcuts_lines, popup_status_lines};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn outgoing_flow() -> VerificationFlow {
        VerificationFlow {
            account_id: Uuid::nil(),
            user_id: "@self:example.com".to_owned(),
            device_id: "DEV".to_owned(),
            flow_id: Some("txn1".to_owned()),
            direction: VerificationDirection::Outgoing,
            stage: VerificationStage::Waiting,
            emoji: None,
            decimals: None,
        }
    }

    fn sas_frame() -> VerificationFrameDto {
        VerificationFrameDto {
            flow_id: "txn1".to_owned(),
            user_id: "@self:example.com".to_owned(),
            device_id: Some("DEV".to_owned()),
            emoji: Some(vec![EmojiDto {
                symbol: "🐶".to_owned(),
                description: "Dog".to_owned(),
            }]),
            decimals: Some([1, 2, 3]),
            reason: None,
        }
    }

    fn flow_dto(flow_id: &str, user_id: &str, device_id: Option<&str>) -> FlowDto {
        FlowDto {
            flow_id: flow_id.to_owned(),
            user_id: user_id.to_owned(),
            device_id: device_id.map(str::to_owned),
            stage: FlowStage::Requested,
            emoji: None,
            decimals: None,
            cancel_reason: None,
        }
    }

    #[test]
    fn send_path_expands_home_slash_for_filesystem_reads() {
        assert_eq!(
            expand_send_path_with_home(
                "~/Downloads/photo.png",
                Some(std::ffi::OsString::from("/home/ada"))
            ),
            PathBuf::from("/home/ada/Downloads/photo.png")
        );
        assert_eq!(
            expand_send_path_with_home(
                "~other/photo.png",
                Some(std::ffi::OsString::from("/home/ada"))
            ),
            PathBuf::from("~other/photo.png")
        );
        assert_eq!(
            expand_send_path_with_home("~/photo.png", None),
            PathBuf::from("~/photo.png")
        );
    }

    #[test]
    fn verification_sas_frame_moves_to_compare() {
        let mut flow = outgoing_flow();
        flow.apply_frame(VerificationFrameKind::Sas, &sas_frame());
        assert_eq!(flow.stage, VerificationStage::Compare);
        assert_eq!(flow.decimals, Some([1, 2, 3]));
        assert_eq!(flow.emoji.as_ref().unwrap()[0].symbol, "🐶");
    }

    #[test]
    fn verification_done_and_cancel_are_terminal() {
        let mut flow = outgoing_flow();
        flow.apply_frame(VerificationFrameKind::Done, &sas_frame());
        assert_eq!(flow.stage, VerificationStage::Done);
        assert!(flow.stage.is_terminal());

        let mut flow = outgoing_flow();
        let cancel = VerificationFrameDto {
            reason: Some("user".to_owned()),
            ..sas_frame()
        };
        flow.apply_frame(VerificationFrameKind::Cancelled, &cancel);
        assert!(matches!(flow.stage, VerificationStage::Ended(_)));
    }

    #[test]
    fn verification_confirming_not_regressed_by_late_sas() {
        // After the user confirms, a trailing SAS frame must not pull the modal
        // back to the compare prompt.
        let mut flow = outgoing_flow();
        flow.stage = VerificationStage::Confirming;
        flow.apply_frame(VerificationFrameKind::Sas, &sas_frame());
        assert_eq!(flow.stage, VerificationStage::Confirming);
    }

    #[test]
    fn pending_outgoing_requested_frame_is_not_treated_as_unsolicited() {
        let mut app = app_with_rooms(Vec::new());
        app.display.accept_incoming_verification = false;
        app.accounts.accounts = vec![account_with_id(
            Uuid::nil(),
            "@alice:example.com",
            AccountState::Active,
        )];
        app.verification = Some(VerificationFlow {
            account_id: Uuid::nil(),
            user_id: "@bob:example.com".to_owned(),
            device_id: String::new(),
            flow_id: None,
            direction: VerificationDirection::Outgoing,
            stage: VerificationStage::Starting,
            emoji: None,
            decimals: None,
        });

        let action = app.handle_live_frame(LiveFrame::Verification(VerificationFrame {
            account_id: Uuid::nil(),
            kind: VerificationFrameKind::Requested,
            payload: VerificationFrameDto {
                flow_id: "server-flow".to_owned(),
                user_id: "@bob:example.com".to_owned(),
                device_id: None,
                emoji: None,
                decimals: None,
                reason: None,
            },
        }));

        assert_eq!(action, LiveFrameAction::None);
        let flow = app.verification.as_ref().unwrap();
        assert_eq!(flow.direction, VerificationDirection::Outgoing);
        assert_eq!(flow.flow_id, None);
        assert_eq!(flow.stage, VerificationStage::Starting);
    }

    #[test]
    fn same_user_frame_does_not_bind_pending_outgoing_without_device_match() {
        let mut app = app_with_rooms(Vec::new());
        app.verification = Some(VerificationFlow {
            account_id: Uuid::nil(),
            user_id: "@bob:example.com".to_owned(),
            device_id: String::new(),
            flow_id: None,
            direction: VerificationDirection::Outgoing,
            stage: VerificationStage::Starting,
            emoji: None,
            decimals: None,
        });

        let action = app.handle_live_frame(LiveFrame::Verification(VerificationFrame {
            account_id: Uuid::nil(),
            kind: VerificationFrameKind::Sas,
            payload: VerificationFrameDto {
                flow_id: "other-flow".to_owned(),
                user_id: "@bob:example.com".to_owned(),
                device_id: None,
                emoji: Some(vec![EmojiDto {
                    symbol: "🐶".to_owned(),
                    description: "Dog".to_owned(),
                }]),
                decimals: Some([1, 2, 3]),
                reason: None,
            },
        }));

        assert_eq!(action, LiveFrameAction::None);
        let flow = app.verification.as_ref().unwrap();
        assert_eq!(flow.flow_id, None);
        assert_eq!(flow.emoji, None);
        assert_eq!(flow.decimals, None);
    }

    #[tokio::test]
    async fn discovered_cross_user_request_honors_incoming_suppression() {
        let mut app = app_with_rooms(Vec::new());
        app.display.accept_incoming_verification = false;
        app.accounts.accounts = vec![account_with_id(
            Uuid::nil(),
            "@alice:example.com",
            AccountState::Active,
        )];

        app.handle_lifecycle_outcome(LifecycleOutcome::VerifyDiscovered {
            account_id: Uuid::nil(),
            result: Ok(vec![flow_dto("flow1", "@bob:example.com", None)]),
        })
        .await;

        assert!(app.verification.is_none());
        assert_ne!(app.mode, Mode::Verification);
    }

    #[test]
    fn verification_apply_flow_maps_server_stage() {
        let mut flow = outgoing_flow();
        flow.apply_flow(&FlowDto {
            flow_id: "txn1".to_owned(),
            user_id: "@self:example.com".to_owned(),
            device_id: Some("DEV".to_owned()),
            stage: FlowStage::KeysExchanged,
            emoji: Some(vec![EmojiDto {
                symbol: "🐱".to_owned(),
                description: "Cat".to_owned(),
            }]),
            decimals: Some([4, 5, 6]),
            cancel_reason: None,
        });
        assert_eq!(flow.stage, VerificationStage::Compare);
        assert!(flow.matches(Uuid::nil(), "txn1"));
        assert!(!flow.matches(Uuid::nil(), "other"));
    }

    fn room(room_id: &str, alias: Option<&str>, name: Option<&str>) -> RoomDto {
        RoomDto {
            account_id: Uuid::nil(),
            account_user_id: Some("@alice:example.com".to_owned()),
            room_id: room_id.to_owned(),
            name: name.map(str::to_owned),
            topic: None,
            avatar_url: None,
            canonical_alias: alias.map(str::to_owned),
            last_activity_ts: 0,
            last_event_id: None,
        }
    }

    fn event_with_id(
        event_id: &str,
        event_type: &str,
        body: Option<&str>,
        content: serde_json::Value,
    ) -> EventDto {
        event_with_state_key(event_id, event_type, None, body, content)
    }

    fn event_with_state_key(
        event_id: &str,
        event_type: &str,
        state_key: Option<&str>,
        body: Option<&str>,
        content: serde_json::Value,
    ) -> EventDto {
        EventDto {
            account_id: Uuid::nil(),
            event_id: event_id.to_owned(),
            room_id: "!room:example.com".to_owned(),
            sender: "@alice:example.com".to_owned(),
            state_key: state_key.map(str::to_owned),
            arrival_order: 0,
            origin_ts: 0,
            event_type: event_type.to_owned(),
            content: Some(content),
            body: body.map(str::to_owned),
            relates_to: None,
            redacted: false,
            redaction_event_id: None,
            reactions: None,
            sender_trust: None,
        }
    }

    fn event(event_type: &str, body: Option<&str>, content: serde_json::Value) -> EventDto {
        event_with_id(
            &format!("${event_type}:example.com"),
            event_type,
            body,
            content,
        )
    }

    fn tally(count: i64, me: bool, my_event_ids: &[&str]) -> crate::api::ReactionTally {
        crate::api::ReactionTally {
            count,
            me,
            my_event_ids: my_event_ids.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// A message event carrying a server-aggregated reaction tally — the M8 shape
    /// the timeline now returns in place of raw `m.reaction` events.
    fn message_with_reactions(
        event_id: &str,
        reactions: Vec<(&str, crate::api::ReactionTally)>,
    ) -> EventDto {
        let mut event = event_with_id(
            event_id,
            "m.room.message",
            Some("message"),
            serde_json::json!({ "msgtype": "m.text", "body": "message" }),
        );
        event.reactions = Some(
            reactions
                .into_iter()
                .map(|(key, tally)| (key.to_owned(), tally))
                .collect(),
        );
        event
    }

    fn app_with_rooms(rooms: Vec<RoomDto>) -> App {
        let mut app = App::new(
            AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            Picker::halfblocks(),
        );
        app.rooms.rooms = rooms;
        app.show_input_help = false;
        app.status = Status::Info(String::new());
        app
    }

    fn seed_room_caches(app: &mut App, key: &RoomKey) {
        app.rooms.unread.insert(key.clone(), 2);
        app.rooms.display_names.insert(key.clone(), HashMap::new());
        app.room_titles
            .insert(key.clone(), "Cached title".to_owned());
        app.messages.events.insert(
            key.clone(),
            vec![event_with_id(
                "$cached",
                "m.room.message",
                Some("hello"),
                serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
            )],
        );
        app.messages
            .history_cursors
            .insert(key.clone(), "cursor".to_owned());
        app.thread_summaries.insert(key.clone(), HashMap::new());
        app.relation_refresh_latest.insert(key.clone(), 1);
        app.members_refresh_after
            .insert(key.clone(), std::time::Instant::now());
        app.unread_threads.insert(key.clone(), HashMap::new());
    }

    fn assert_room_caches_pruned(app: &App, key: &RoomKey) {
        assert!(!app.rooms.unread.contains_key(key));
        assert!(!app.rooms.display_names.contains_key(key));
        assert!(!app.room_titles.contains_key(key));
        assert!(!app.messages.events.contains_key(key));
        assert!(!app.messages.history_cursors.contains_key(key));
        assert!(!app.thread_summaries.contains_key(key));
        assert!(!app.relation_refresh_latest.contains_key(key));
        assert!(!app.members_refresh_after.contains_key(key));
        assert!(!app.unread_threads.contains_key(key));
    }

    async fn spawn_api_stub(responses: Vec<String>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test API stub");
        let address = listener.local_addr().expect("test API stub address");
        tokio::spawn(async move {
            for body in responses {
                let (mut socket, _) = listener.accept().await.expect("accept API request");
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                loop {
                    let read = socket.read(&mut buffer).await.expect("read API request");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write API response");
            }
        });
        format!("http://{address}")
    }

    fn rooms_response_body(rooms: &[RoomDto]) -> String {
        let data: Vec<_> = rooms
            .iter()
            .map(|room| {
                serde_json::json!({
                    "account_id": room.account_id,
                    "account_user_id": room.account_user_id,
                    "room_id": room.room_id,
                    "name": room.name,
                    "topic": room.topic,
                    "avatar_url": room.avatar_url,
                    "canonical_alias": room.canonical_alias,
                    "last_activity_ts": room.last_activity_ts,
                    "last_event_id": room.last_event_id,
                })
            })
            .collect();
        serde_json::json!({ "data": data }).to_string()
    }

    fn empty_timeline_response_body() -> String {
        serde_json::json!({
            "data": {
                "events": [],
                "next_cursor": null,
            }
        })
        .to_string()
    }

    fn empty_members_response_body() -> String {
        serde_json::json!({ "data": [] }).to_string()
    }

    #[test]
    fn visible_room_indices_filters_dms_groups_unread_and_favorites() {
        // index 0: a DM (no name/alias); 1: a named group; 2: another DM.
        let dm1 = room("!dm1:example.com", None, None);
        let group = room("!g:example.com", None, Some("Team"));
        let dm2 = room("!dm2:example.com", None, None);
        let mut app = app_with_rooms(vec![dm1.clone(), group.clone(), dm2.clone()]);
        // No selection, so the "keep selected visible" rule never interferes.
        app.rooms.selected = None;

        app.room_filter = RoomFilter::All;
        assert_eq!(app.visible_room_indices(), vec![0, 1, 2]);

        app.room_filter = RoomFilter::Dms;
        assert_eq!(app.visible_room_indices(), vec![0, 2]);

        app.room_filter = RoomFilter::Groups;
        assert_eq!(app.visible_room_indices(), vec![1]);

        // Mark the group unread; only it should show under the unread filter.
        app.rooms.unread.insert(RoomKey::from(&group), 4);
        app.room_filter = RoomFilter::Unread;
        assert_eq!(app.visible_room_indices(), vec![1]);

        // Pin dm2; favorites shows only pinned rooms.
        app.pinned_rooms = vec![RoomKey::from(&dm2)];
        app.room_filter = RoomFilter::Favorites;
        assert_eq!(app.visible_room_indices(), vec![2]);

        // Name filter matches on the group's name, case-insensitively.
        app.room_filter = RoomFilter::Name("team".to_owned());
        assert_eq!(app.visible_room_indices(), vec![1]);
    }

    #[test]
    fn room_filter_name_cycles_to_all() {
        assert_eq!(RoomFilter::Name("team".to_owned()).next(), RoomFilter::All);
    }

    #[test]
    fn name_filter_matches_member_derived_room_title() {
        let dm = room("!dm:example.com", None, None);
        let group = room("!g:example.com", None, Some("Team"));
        let mut app = app_with_rooms(vec![dm.clone(), group]);
        app.rooms.selected = None;
        app.room_titles
            .insert(RoomKey::from(&dm), "Alice Example".to_owned());

        app.room_filter = RoomFilter::Name("alice".to_owned());

        assert_eq!(app.visible_room_indices(), vec![0]);
    }

    #[test]
    fn cancel_reediting_name_filter_restores_existing_name_filter() {
        let mut app = app_with_rooms(Vec::new());
        app.room_filter = RoomFilter::Name("team".to_owned());

        app.begin_room_name_filter();
        app.update_room_name_filter("te".to_owned());
        app.cancel_room_name_filter();

        assert_eq!(app.room_filter, RoomFilter::Name("team".to_owned()));
    }

    #[test]
    fn date_jump_counts_as_mid_command() {
        let mut app = app_with_rooms(Vec::new());
        app.mode = Mode::DateJump;

        assert!(app.is_mid_command());
    }

    #[tokio::test]
    async fn date_jump_prompt_ignores_message_navigation_shortcuts() {
        let mut app = app_with_rooms(Vec::new());
        app.mode = Mode::DateJump;
        app.input.buffer = "2026-06-25".to_owned();
        app.input.cursor = app.input.buffer.len();

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
            .await;

        assert_eq!(app.mode, Mode::DateJump);
        assert_eq!(app.input.buffer, "2026-06-25");
    }

    #[test]
    fn visible_room_indices_always_keeps_selected_room_visible() {
        let dm = room("!dm:example.com", None, None);
        let group = room("!g:example.com", None, Some("Team"));
        let mut app = app_with_rooms(vec![dm, group]);
        // Select the DM, then apply a Groups filter that would hide it.
        app.rooms.selected = Some(0);
        app.room_filter = RoomFilter::Groups;
        // The selected DM stays visible alongside the matching group.
        assert_eq!(app.visible_room_indices(), vec![0, 1]);
    }

    fn account(user_id: &str, state: AccountState) -> AccountDto {
        account_with_id(Uuid::from_u128(1), user_id, state)
    }

    fn account_with_id(account_id: Uuid, user_id: &str, state: AccountState) -> AccountDto {
        AccountDto {
            account_id,
            user_id: user_id.to_owned(),
            state,
            device_id: None,
            verified: Some(false),
        }
    }

    #[test]
    fn media_cache_keys_are_account_scoped() {
        let url = "mxc://example.com/media".to_owned();

        assert_ne!(
            MediaKey::new(Uuid::from_u128(1), url.clone()),
            MediaKey::new(Uuid::from_u128(2), url)
        );
    }

    #[test]
    fn bounded_cache_never_evicts_in_flight_work() {
        let mut cache = HashMap::from([("ready".to_owned(), false), ("fetching".to_owned(), true)]);
        let mut order = VecDeque::from(["ready".to_owned(), "fetching".to_owned()]);

        assert!(evict_lru_where(&mut cache, &mut order, 2, |in_flight| {
            !*in_flight
        }));
        assert!(!cache.contains_key("ready"));
        assert!(cache.contains_key("fetching"));

        cache.insert("encoding".to_owned(), true);
        order.push_back("encoding".to_owned());
        assert!(!evict_lru_where(&mut cache, &mut order, 2, |in_flight| {
            !*in_flight
        }));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn account_refresh_preserves_selected_account_by_id() {
        let first_id = Uuid::from_u128(1);
        let selected_id = Uuid::from_u128(2);
        let added_id = Uuid::from_u128(3);
        let mut app = app_with_rooms(Vec::new());
        app.set_accounts(vec![
            account_with_id(first_id, "@first:example.com", AccountState::Active),
            account_with_id(selected_id, "@selected:example.com", AccountState::Active),
        ]);
        app.accounts.selected = AccountSelection::Account(1);

        app.set_accounts(vec![
            account_with_id(selected_id, "@selected:example.com", AccountState::Active),
            account_with_id(added_id, "@added:example.com", AccountState::Active),
        ]);

        assert_eq!(app.active_account_filter(), Some(selected_id));
        assert_eq!(app.accounts.selected, AccountSelection::Account(0));
    }

    #[test]
    fn cli_account_filter_restricts_account_navigation_state() {
        let filter_id = Uuid::from_u128(1);
        let other_id = Uuid::from_u128(2);
        let mut app = App::new(
            AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            Some(filter_id),
            TuiConfig::test_default(),
            Picker::halfblocks(),
        );

        app.set_accounts(vec![
            account_with_id(filter_id, "@filtered:example.com", AccountState::Active),
            account_with_id(other_id, "@other:example.com", AccountState::Active),
        ]);

        assert_eq!(app.accounts.accounts.len(), 1);
        assert_eq!(app.accounts.accounts[0].account_id, filter_id);
    }

    #[test]
    fn room_refresh_preserves_selected_room_by_key() {
        let first = room("!one:example.com", Some("#one:example.com"), Some("One"));
        let second = room("!two:example.com", Some("#two:example.com"), Some("Two"));
        let mut app = app_with_rooms(vec![first.clone(), second.clone()]);
        app.rooms.selected = Some(1);

        app.apply_room_refresh(vec![second.clone(), first]);

        assert_eq!(
            app.selected_room().map(|room| room.room_id.as_str()),
            Some("!two:example.com")
        );
        assert_eq!(app.rooms.selected, Some(0));
    }

    #[test]
    fn room_refresh_drops_rooms_for_logged_out_accounts() {
        let active_id = Uuid::from_u128(1);
        let logged_out_id = Uuid::from_u128(2);

        let mut active_room = room("!active:example.com", None, Some("Active"));
        active_room.account_id = active_id;
        let mut stale_room = room("!stale:example.com", None, Some("Stale"));
        stale_room.account_id = logged_out_id;

        let mut app = app_with_rooms(Vec::new());
        app.set_accounts(vec![
            AccountDto {
                account_id: active_id,
                user_id: "@alice:example.com".to_owned(),
                state: AccountState::Active,
                device_id: None,
                verified: Some(false),
            },
            AccountDto {
                account_id: logged_out_id,
                user_id: "@bob:example.com".to_owned(),
                state: AccountState::Deactivated,
                device_id: None,
                verified: Some(false),
            },
        ]);

        app.apply_room_refresh(vec![active_room, stale_room]);

        assert_eq!(
            app.rooms
                .rooms
                .iter()
                .map(|room| room.room_id.as_str())
                .collect::<Vec<_>>(),
            vec!["!active:example.com"]
        );
    }

    #[test]
    fn room_refresh_prunes_caches_for_rooms_that_drop_out() {
        let kept = room("!kept:example.com", None, Some("Kept"));
        let departed = room("!departed:example.com", None, Some("Departed"));
        let departed_key = RoomKey::from(&departed);
        let mut app = app_with_rooms(vec![kept.clone(), departed]);
        app.rooms.selected = Some(1);
        seed_room_caches(&mut app, &departed_key);

        app.apply_room_refresh(vec![kept]);

        assert_room_caches_pruned(&app, &departed_key);
        assert_eq!(
            app.selected_room().map(|room| room.room_id.as_str()),
            Some("!kept:example.com")
        );
    }

    #[tokio::test]
    async fn leave_outcome_prunes_departed_caches_after_post_leave_refresh() {
        let departed = room("!departed:example.com", None, Some("Departed"));
        let next = room("!next:example.com", None, Some("Next"));
        let departed_key = RoomKey::from(&departed);
        let base_url = spawn_api_stub(vec![
            rooms_response_body(std::slice::from_ref(&next)),
            empty_timeline_response_body(),
            empty_members_response_body(),
        ])
        .await;
        let mut app = App::new(
            AxonClient::new(base_url, None),
            None,
            TuiConfig::test_default(),
            Picker::halfblocks(),
        );
        app.rooms.rooms = vec![departed.clone(), next.clone()];
        app.rooms.selected = Some(0);
        seed_room_caches(&mut app, &departed_key);

        app.handle_room_action_outcome(RoomActionOutcome {
            action: PendingRoomAction {
                kind: super::room_actions::RoomActionKind::Leave,
                key: departed_key.clone(),
                room_title: departed.title().to_owned(),
                user_id: None,
                reason: None,
            },
            result: Ok(()),
        })
        .await;

        assert_eq!(
            app.rooms
                .rooms
                .iter()
                .map(|room| room.room_id.as_str())
                .collect::<Vec<_>>(),
            vec!["!next:example.com"]
        );
        assert_eq!(
            app.selected_room().map(|room| room.room_id.as_str()),
            Some("!next:example.com")
        );
        assert_room_caches_pruned(&app, &departed_key);
        assert_eq!(app.status, "left Departed");
    }

    #[test]
    fn status_lists_active_and_deactivated_accounts() {
        let active_id = Uuid::from_u128(1);
        let logged_out_id = Uuid::from_u128(2);
        let mut app = app_with_rooms(Vec::new());
        app.set_accounts(vec![
            account_with_id(active_id, "@alice:example.com", AccountState::Active),
            account_with_id(logged_out_id, "@bob:example.com", AccountState::Deactivated),
        ]);

        assert_eq!(
            app.accounts
                .accounts
                .iter()
                .map(|account| account.account_id)
                .collect::<Vec<_>>(),
            vec![active_id],
            "active navigation remains active-only"
        );

        let status = popup_status_lines(&app).join("\n");
        assert!(status.contains("@alice:example.com  (logged in, 0 rooms)"));
        assert!(status.contains("@bob:example.com  (logged out, 0 rooms)"));
    }

    #[test]
    fn status_disambiguates_duplicate_matrix_ids_with_account_ids() {
        let first_id = Uuid::from_u128(1);
        let second_id = Uuid::from_u128(2);
        let mut app = app_with_rooms(Vec::new());
        app.set_accounts(vec![
            account_with_id(first_id, "@alice:example.com", AccountState::Active),
            account_with_id(second_id, "@alice:example.com", AccountState::Deactivated),
        ]);

        let status = popup_status_lines(&app).join("\n");
        assert!(status.contains(&format!("@alice:example.com  [{first_id}]  (logged in")));
        assert!(status.contains(&format!("@alice:example.com  [{second_id}]  (logged out")));
    }

    #[test]
    fn room_refresh_keeps_rooms_for_accounts_not_yet_listed() {
        // An empty/stale account list must not blank the whole room list.
        let mut unknown_room = room("!unknown:example.com", None, Some("Unknown"));
        unknown_room.account_id = Uuid::from_u128(9);
        let mut app = app_with_rooms(Vec::new());

        app.apply_room_refresh(vec![unknown_room]);

        assert_eq!(app.rooms.rooms.len(), 1);
    }

    #[test]
    fn filtered_room_refresh_does_not_select_a_hidden_room() {
        let visible_account = Uuid::from_u128(1);
        let other_account = Uuid::from_u128(2);
        let mut other_room = room("!other:example.com", None, Some("Other"));
        other_room.account_id = other_account;
        let mut app = app_with_rooms(Vec::new());
        app.set_accounts(vec![
            account_with_id(
                visible_account,
                "@visible:example.com",
                AccountState::Active,
            ),
            account_with_id(other_account, "@other:example.com", AccountState::Active),
        ]);
        app.accounts.selected = AccountSelection::Account(0);

        app.apply_room_refresh(vec![other_room]);

        assert_eq!(app.rooms.selected, None);
        assert!(app.selected_room().is_none());
    }

    #[test]
    fn live_event_for_unknown_room_requests_room_refresh() {
        let mut app = app_with_rooms(Vec::new());
        let event = event_with_id(
            "$new:example.com",
            "m.room.message",
            Some("hello"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
        );

        let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(event)));

        assert_eq!(action, LiveFrameAction::RefreshRooms);
    }

    /// ADR 0089, live path. A backfilled event delivered while the room is on
    /// screen is older by `origin_ts` than the marker already holds, so the
    /// marker correctly refuses it — but it arrived last, so it is exactly what
    /// the receipt must name. The receipt has to advance on its own.
    #[test]
    fn live_event_advances_the_receipt_target_when_the_marker_refuses() {
        let mut app = app_with_rooms(vec![room(
            "!room:example.com",
            Some("#room:example.com"),
            Some("Room"),
        )]);
        app.rooms.selected = Some(0);
        let key = RoomKey {
            account_id: Uuid::nil(),
            room_id: "!room:example.com".to_owned(),
        };
        app.read_markers.insert(
            key.clone(),
            read_markers::ReadMarker {
                event_id: "$bridge".to_owned(),
                origin_ts: 1_785_928_309_453,
            },
        );

        let mut event = event_with_id(
            "$backfilled:example.com",
            "m.room.message",
            Some("hello"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
        );
        event.origin_ts = 1_785_928_304_987;
        event.arrival_order = 1_871_426;
        app.handle_live_frame(LiveFrame::Timeline(Box::new(event)));

        assert_eq!(app.read_markers.get(&key).unwrap().event_id, "$bridge");
        assert_eq!(
            app.receipt_targets.get(&key).unwrap().event_id,
            "$backfilled:example.com"
        );
    }

    /// The `event_shown` gate governs both halves: an event the user cannot see
    /// is neither a marker position nor a receipt target.
    #[test]
    fn hidden_live_event_advances_neither_position() {
        let mut app = app_with_rooms(vec![room(
            "!room:example.com",
            Some("#room:example.com"),
            Some("Room"),
        )]);
        app.rooms.selected = Some(0);
        let key = RoomKey {
            account_id: Uuid::nil(),
            room_id: "!room:example.com".to_owned(),
        };

        let mut event = event_with_id(
            "$reaction:example.com",
            "m.reaction",
            None,
            serde_json::json!({
                "m.relates_to": { "rel_type": "m.annotation", "event_id": "$t", "key": "👍" }
            }),
        );
        event.origin_ts = 5_000;
        event.arrival_order = 9_999;
        app.handle_live_frame(LiveFrame::Timeline(Box::new(event)));

        assert!(!app.read_markers.contains_key(&key));
        assert!(!app.receipt_targets.contains_key(&key));
    }

    #[test]
    fn live_event_for_known_unselected_room_only_updates_unread() {
        let mut app = app_with_rooms(vec![room(
            "!room:example.com",
            Some("#room:example.com"),
            Some("Room"),
        )]);
        let event = event_with_id(
            "$known:example.com",
            "m.room.message",
            Some("hello"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
        );

        let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(event)));

        assert_eq!(action, LiveFrameAction::None);
        assert_eq!(
            app.rooms
                .unread
                .get(&RoomKey {
                    account_id: Uuid::nil(),
                    room_id: "!room:example.com".to_owned(),
                })
                .copied(),
            Some(1)
        );
    }

    #[test]
    fn own_live_leave_event_for_selected_room_requests_room_refresh() {
        let mut room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        room.account_user_id = Some("@me:example.com".to_owned());
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        let mut event = event_with_state_key(
            "$leave:example.com",
            "m.room.member",
            Some("@me:example.com"),
            None,
            serde_json::json!({ "membership": "leave" }),
        );
        event.room_id = room.room_id.clone();

        let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(event)));

        assert_eq!(action, LiveFrameAction::RefreshRooms);
    }

    #[test]
    fn own_live_ban_event_for_unselected_room_requests_room_refresh() {
        let mut target = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        target.account_user_id = Some("@me:example.com".to_owned());
        let mut other = room(
            "!other:example.com",
            Some("#other:example.com"),
            Some("Other"),
        );
        other.account_user_id = Some("@me:example.com".to_owned());
        let mut app = app_with_rooms(vec![other, target.clone()]);
        app.rooms.selected = Some(0);
        let mut event = event_with_state_key(
            "$ban:example.com",
            "m.room.member",
            Some("@me:example.com"),
            None,
            serde_json::json!({ "membership": "ban" }),
        );
        event.room_id = target.room_id.clone();

        let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(event)));

        assert_eq!(action, LiveFrameAction::RefreshRooms);
    }

    #[test]
    fn peer_live_leave_event_for_known_room_does_not_request_room_refresh() {
        let mut room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        room.account_user_id = Some("@me:example.com".to_owned());
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        let mut event = event_with_state_key(
            "$leave:example.com",
            "m.room.member",
            Some("@peer:example.com"),
            None,
            serde_json::json!({ "membership": "leave" }),
        );
        event.room_id = room.room_id.clone();

        let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(event)));

        assert_eq!(action, LiveFrameAction::None);
    }

    #[test]
    fn live_formatted_edit_replaces_rendered_content() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        let mut original = event_with_id(
            "$original:example.com",
            "m.room.message",
            Some("hello"),
            serde_json::json!({
                "msgtype": "m.text",
                "body": "hello",
                "format": "org.matrix.custom.html",
                "formatted_body": "<em>hello</em>"
            }),
        );
        original.room_id = room.room_id.clone();
        app.messages
            .events
            .insert(RoomKey::from(&room), vec![original]);

        let mut edit = event_with_id(
            "$edit:example.com",
            "m.room.message",
            Some("* hello world"),
            serde_json::json!({
                "msgtype": "m.text",
                "body": "* hello world",
                "m.new_content": {
                    "msgtype": "m.text",
                    "body": "hello world",
                    "format": "org.matrix.custom.html",
                    "formatted_body": "<strong>hello world</strong>"
                }
            }),
        );
        edit.room_id = room.room_id.clone();
        edit.relates_to = Some(serde_json::json!({
            "rel_type": "m.replace",
            "event_id": "$original:example.com"
        }));

        let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(edit)));

        assert_eq!(action, LiveFrameAction::None);
        let updated = &app.messages.events[&RoomKey::from(&room)][0];
        assert_eq!(updated.body.as_deref(), Some("hello world"));
        assert_eq!(
            updated.formatted_body(),
            Some("<strong>hello world</strong>")
        );
    }

    #[test]
    fn hidden_live_event_for_known_unselected_room_does_not_update_unread() {
        let mut app = app_with_rooms(vec![room(
            "!room:example.com",
            Some("#room:example.com"),
            Some("Room"),
        )]);
        let event = event_with_id(
            "$reaction:example.com",
            "m.reaction",
            None,
            serde_json::json!({
                "m.relates_to": {
                    "rel_type": "m.annotation",
                    "event_id": "$known:example.com",
                    "key": "👍"
                }
            }),
        );

        let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(event)));

        assert_eq!(action, LiveFrameAction::None);
        assert_eq!(
            app.rooms.unread.get(&RoomKey {
                account_id: Uuid::nil(),
                room_id: "!room:example.com".to_owned(),
            }),
            None
        );
    }

    #[test]
    pub(crate) fn find_room_matches_incomplete_alias_localpart() {
        let app = app_with_rooms(vec![room(
            "!abc:example.com",
            Some("#test:example.com"),
            Some("Test Room"),
        )]);

        assert_eq!(
            app.resolve_room_target("test"),
            RoomTargetResolution::Match(0)
        );
        assert_eq!(
            app.resolve_room_target("#test"),
            RoomTargetResolution::Match(0)
        );
        assert_eq!(
            app.resolve_room_target("TEST"),
            RoomTargetResolution::Match(0)
        );
    }

    #[test]
    pub(crate) fn find_room_matches_one_based_room_list_number() {
        let app = app_with_rooms(vec![
            room("!one:example.com", Some("#one:example.com"), Some("One")),
            room("!two:example.com", Some("#two:example.com"), Some("Two")),
        ]);

        assert_eq!(app.resolve_room_target("1"), RoomTargetResolution::Match(0));
        assert_eq!(app.resolve_room_target("2"), RoomTargetResolution::Match(1));
        assert_eq!(app.resolve_room_target("0"), RoomTargetResolution::Missing);
        assert_eq!(app.resolve_room_target("3"), RoomTargetResolution::Missing);
    }

    #[test]
    fn room_resolution_ignores_rooms_hidden_by_account_filter() {
        let visible_account = Uuid::from_u128(1);
        let hidden_account = Uuid::from_u128(2);
        let mut visible = room("!visible:example.com", None, Some("General"));
        visible.account_id = visible_account;
        let mut hidden = room("!hidden:example.com", None, Some("General"));
        hidden.account_id = hidden_account;
        let mut app = app_with_rooms(vec![visible, hidden]);
        app.set_accounts(vec![
            account_with_id(
                visible_account,
                "@visible:example.com",
                AccountState::Active,
            ),
            account_with_id(hidden_account, "@hidden:example.com", AccountState::Active),
        ]);
        app.accounts.selected = AccountSelection::Account(0);

        assert_eq!(
            app.resolve_room_target("General"),
            RoomTargetResolution::Match(0)
        );
    }

    #[test]
    pub(crate) fn relative_room_index_wraps_next_and_previous() {
        assert_eq!(relative_room_index(0, 3, 1), 1);
        assert_eq!(relative_room_index(2, 3, 1), 0);
        assert_eq!(relative_room_index(1, 3, -1), 0);
        assert_eq!(relative_room_index(0, 3, -1), 2);
    }

    #[test]
    fn event_filter_hides_state_events_but_keeps_membership() {
        let mut display = DisplayOptions {
            debug: false,
            show_state_events: false,
            message_density: MessageDensity::Normal,
            time_format: TimeFormat::H24,
            input_lines: 1,
            max_input_lines: None,
            preview_warmup_count: 5,
            highlight_selected_line: false,
            confirm_logout: true,
            search_wrap: true,
            accept_incoming_verification: true,
            accounts_panel_width: 25,
            rooms_panel_width_adj: 0,
            pinned_rooms: Vec::new(),
            room_sort: "recent".to_owned(),
            room_filter: "all".to_owned(),
        };
        let state = event_with_state_key(
            "$m.room.topic:example.com",
            "m.room.topic",
            Some(""),
            None,
            serde_json::json!({ "topic": "new topic" }),
        );
        let membership = event_with_state_key(
            "$m.room.member:example.com",
            "m.room.member",
            Some("@alice:example.com"),
            None,
            serde_json::json!({ "membership": "join" }),
        );
        let message = event(
            "m.room.message",
            Some("hello"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
        );
        let utd = EventDto {
            content: None,
            body: None,
            event_type: "m.room.encrypted".to_owned(),
            ..event("m.room.encrypted", None, serde_json::json!({}))
        };

        assert!(!should_show_event(&state, &display));
        assert!(should_show_event(&membership, &display));
        assert!(should_show_event(&message, &display));
        assert!(should_show_event(&utd, &display));

        display.show_state_events = true;
        assert!(should_show_event(&state, &display));
    }

    #[test]
    pub(crate) fn sender_label_defaults_to_membership_display_name() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        let membership = EventDto {
            sender: "@jamie:example.com".to_owned(),
            ..event_with_state_key(
                "$member:example.com",
                "m.room.member",
                Some("@alice:example.com"),
                None,
                serde_json::json!({
                    "membership": "join",
                    "displayname": "Alice"
                }),
            )
        };
        app.rebuild_display_names(&room, &[membership]);
        let message = event_with_id(
            "$message:example.com",
            "m.room.message",
            Some("hello"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
        );

        assert_eq!(app.sender_label(&message), "Alice");
    }

    #[test]
    pub(crate) fn sender_label_prefers_display_name_in_both_densities() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        let membership = event_with_state_key(
            "$member:example.com",
            "m.room.member",
            Some("@alice:example.com"),
            None,
            serde_json::json!({
                "membership": "join",
                "displayname": "Alice"
            }),
        );
        app.rebuild_display_names(&room, &[membership]);
        let message = event_with_id(
            "$message:example.com",
            "m.room.message",
            Some("hello"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
        );

        // A known display name is shown regardless of layout density.
        app.display.message_density = MessageDensity::Normal;
        assert_eq!(app.sender_label(&message), "Alice");
        app.display.message_density = MessageDensity::Dense;
        assert_eq!(app.sender_label(&message), "Alice");
    }

    #[test]
    pub(crate) fn incremental_history_keeps_existing_display_names() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.seed_display_names_from_members(
            &room,
            &[MemberDto {
                user_id: "@alice:example.com".to_owned(),
                display_name: Some("Current Alice".to_owned()),
            }],
        );
        let stale_membership = event_with_state_key(
            "$older-member:example.com",
            "m.room.member",
            Some("@alice:example.com"),
            None,
            serde_json::json!({
                "membership": "join",
                "displayname": "Old Alice"
            }),
        );

        app.merge_missing_display_names_from_events(&room, &[stale_membership]);

        let message = event_with_id(
            "$message:example.com",
            "m.room.message",
            Some("hello"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
        );
        assert_eq!(app.sender_label(&message), "Current Alice");
    }

    #[test]
    pub(crate) fn sender_label_without_display_name_varies_by_density() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        // No membership event, so no display name is known for the sender.
        let message = event_with_id(
            "$message:example.com",
            "m.room.message",
            Some("hello"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
        );

        // Normal mode shows the full mxid; dense mode drops the homeserver and
        // keeps the `@localpart`.
        app.display.message_density = MessageDensity::Normal;
        assert_eq!(app.sender_label(&message), "@alice:example.com");
        app.display.message_density = MessageDensity::Dense;
        assert_eq!(app.sender_label(&message), "@alice");
    }

    #[test]
    pub(crate) fn members_outcome_resolves_previously_unknown_sender() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.display.message_density = MessageDensity::Normal;
        let message = event_with_id(
            "$message:example.com",
            "m.room.message",
            Some("hello"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
        );
        // No name known yet — falls back to the raw mxid.
        assert_eq!(app.sender_label(&message), "@alice:example.com");

        // A background /members refresh lands and resolves the name in place.
        app.apply_members_outcome(timeline::MembersOutcome {
            room_key: RoomKey::from(&room),
            members: vec![MemberDto {
                user_id: "@alice:example.com".to_owned(),
                display_name: Some("Alice".to_owned()),
            }],
        });
        assert_eq!(app.sender_label(&message), "Alice");
    }

    #[test]
    pub(crate) fn members_refresh_is_skipped_within_cooldown() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        let (tx, _rx) = mpsc::unbounded_channel();
        app.set_members_sender(tx);
        let key = RoomKey::from(&room);
        // Pre-arm a future cooldown deadline. A refresh inside the window must be
        // skipped at the gate — if it fell through to tokio::spawn it would panic
        // outside a runtime, and the deadline would be overwritten.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        app.members_refresh_after.insert(key.clone(), deadline);
        app.spawn_members_refresh(key.clone());
        assert_eq!(app.members_refresh_after.get(&key), Some(&deadline));
    }

    #[test]
    fn whereami_adds_dm_name_line_for_unnamed_room() {
        // An unnamed room (no name/alias) behaves as a DM.
        let dm = room("!dm:example.com", None, None);
        let mut app = app_with_rooms(vec![dm.clone()]);
        app.rooms.selected = Some(0);

        // No derived title yet → no DM name line.
        let before = crate::ui::popup_room_info_lines(&app);
        assert!(!before.iter().any(|line| line.starts_with("DM name:")));

        // Once a /members fetch resolves the partner, the line appears right after
        // "Name:" without removing any existing information.
        app.room_titles
            .insert(RoomKey::from(&dm), "jamie".to_owned());
        let after = crate::ui::popup_room_info_lines(&app);
        assert!(
            after.iter().any(|line| line == "DM name: jamie"),
            "lines: {after:?}"
        );
        assert!(after.iter().any(|line| line.starts_with("Matrix ID:")));
    }

    #[test]
    fn dm_title_prefers_other_members_name() {
        let members = vec![
            MemberDto {
                user_id: "@me:example.com".to_owned(),
                display_name: Some("Me".to_owned()),
            },
            MemberDto {
                user_id: "@jamie:bostoncoop.net".to_owned(),
                display_name: Some("jamie".to_owned()),
            },
        ];
        assert_eq!(
            dm_title_from_members(Some("@me:example.com"), &members).as_deref(),
            Some("jamie")
        );
    }

    #[test]
    fn dm_title_falls_back_to_localpart_without_display_name() {
        let members = vec![
            MemberDto {
                user_id: "@me:example.com".to_owned(),
                display_name: None,
            },
            MemberDto {
                user_id: "@jamie:bostoncoop.net".to_owned(),
                display_name: None,
            },
        ];
        assert_eq!(
            dm_title_from_members(Some("@me:example.com"), &members).as_deref(),
            Some("@jamie")
        );
    }

    #[test]
    fn dm_title_summarizes_large_rooms_and_skips_note_to_self() {
        let mk = |uid: &str, name: &str| MemberDto {
            user_id: uid.to_owned(),
            display_name: Some(name.to_owned()),
        };
        // Only self → None, so the caller keeps the room id fallback.
        let solo = vec![mk("@me:example.com", "Me")];
        assert_eq!(dm_title_from_members(Some("@me:example.com"), &solo), None);

        // Four others → first three (sorted by user id) plus "+1".
        let many = vec![
            mk("@me:example.com", "Me"),
            mk("@a:x", "Al"),
            mk("@b:x", "Bo"),
            mk("@c:x", "Ci"),
            mk("@d:x", "Di"),
        ];
        assert_eq!(
            dm_title_from_members(Some("@me:example.com"), &many).as_deref(),
            Some("Al, Bo, Ci, +1")
        );
    }

    #[test]
    fn own_sender_is_known_from_room_summary_before_first_send() {
        let account_id = Uuid::from_u128(1);
        let mut room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        room.account_id = account_id;
        room.account_user_id = Some("@me:example.com".to_owned());
        let mut app = app_with_rooms(vec![room]);

        app.seed_own_senders_from_rooms();

        assert_eq!(
            app.live.own_senders.get(&account_id).map(String::as_str),
            Some("@me:example.com")
        );
    }

    #[test]
    fn room_summary_without_own_sender_still_loads() {
        let account_id = Uuid::from_u128(3);
        let mut room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        room.account_id = account_id;
        room.account_user_id = None;
        let mut app = app_with_rooms(vec![room]);

        app.seed_own_senders_from_rooms();

        assert!(!app.live.own_senders.contains_key(&account_id));
    }

    #[test]
    fn own_message_color_applies_without_send_echo() {
        let account_id = Uuid::from_u128(2);
        let colors = TuiConfig::test_default().colors;
        let event = EventDto {
            account_id,
            sender: "@me:example.com".to_owned(),
            ..event_with_id(
                "$message:example.com",
                "m.room.message",
                Some("hello"),
                serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
            )
        };
        let sender_labels = vec!["@me:example.com".to_owned()];
        let own_senders = HashMap::from([(account_id, "@me:example.com".to_owned())]);
        let lines = message_layout(
            &[&event],
            sender_labels.as_slice(),
            None,
            &colors,
            80,
            &HashMap::new(),
            &own_senders,
            &ImageThumbRows::new(),
            &RelationContext::default(),
            MessageDensity::Dense,
            TimeFormat::H24,
            false,
        )
        .lines;

        assert_eq!(lines[1].spans[2].style.fg, Some(colors.own_message_sender));
    }

    #[test]
    fn selected_message_background_applies_only_to_first_line_when_enabled() {
        let colors = TuiConfig::test_default().colors;
        let event = event_with_id(
            "$message:example.com",
            "m.room.message",
            Some("hello"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
        );
        let sender_labels = vec!["@alice:example.com".to_owned()];
        let lines = message_layout(
            &[&event],
            sender_labels.as_slice(),
            Some("$message:example.com"),
            &colors,
            80,
            &HashMap::new(),
            &HashMap::new(),
            &ImageThumbRows::new(),
            &RelationContext::default(),
            MessageDensity::Normal,
            TimeFormat::H24,
            true,
        )
        .lines;

        assert_eq!(lines[1].style.bg, Some(colors.selection_background));
        assert_eq!(lines[1].width(), 80);
        assert_eq!(lines[2].style.bg, None);
    }

    #[tokio::test]
    async fn whoami_shows_current_user_id_and_display_name() {
        let mut room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        room.account_user_id = Some("@me:example.com".to_owned());
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        let membership = event_with_state_key(
            "$member:example.com",
            "m.room.member",
            Some("@me:example.com"),
            None,
            serde_json::json!({
                "membership": "join",
                "displayname": "Me Myself"
            }),
        );
        app.rebuild_display_names(&room, &[membership]);

        app.handle_command(Command::Whoami).await;

        assert_eq!(
            app.status.text(false),
            "Matrix ID: @me:example.com; Display Name: Me Myself"
        );
    }

    #[tokio::test]
    async fn whoami_reports_unknown_display_name() {
        let mut room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        room.account_user_id = Some("@me:example.com".to_owned());
        let mut app = app_with_rooms(vec![room]);
        app.rooms.selected = Some(0);

        app.handle_command(Command::Whoami).await;

        assert_eq!(
            app.status.text(false),
            "Matrix ID: @me:example.com; Display Name: unknown"
        );
    }

    #[tokio::test]
    async fn whoami_requires_selected_room_with_user_id() {
        let mut app = app_with_rooms(Vec::new());

        app.handle_command(Command::Whoami).await;

        assert_eq!(app.status.text(false), "select a room before using /whoami");

        let mut room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        room.account_user_id = None;
        app.rooms.rooms = vec![room];
        app.rooms.selected = Some(0);

        app.handle_command(Command::Whoami).await;

        assert_eq!(
            app.status.text(false),
            "current user is unavailable for this room"
        );
    }

    #[tokio::test]
    async fn whereami_opens_room_info_popup_for_selected_room() {
        let mut app = app_with_rooms(vec![room(
            "!room:example.com",
            Some("#room:example.com"),
            Some("Room"),
        )]);
        app.rooms.selected = Some(0);
        app.popup_scroll = 4;

        app.handle_command(Command::Whereami).await;

        assert_eq!(app.mode, Mode::Popup(PopupKind::RoomInfo));
        assert_eq!(app.popup_scroll, 0);
    }

    #[tokio::test]
    async fn whereami_requires_selected_room() {
        let mut app = app_with_rooms(Vec::new());

        app.handle_command(Command::Whereami).await;

        assert_eq!(
            app.status.text(false),
            "select a room before using /whereami"
        );
        assert_eq!(app.mode, Mode::Compose);
    }

    #[tokio::test]
    async fn unsupported_and_unknown_commands_report_distinct_statuses() {
        let mut app = app_with_rooms(Vec::new());

        app.handle_command(Command::ApiUnsupported(
            "/join is not supported by the current Axon API".to_owned(),
        ))
        .await;
        assert_eq!(
            app.status.text(false),
            "/join is not supported by the current Axon API"
        );

        app.handle_command(Command::Unknown("unknown command: /frobnicate".to_owned()))
            .await;
        assert_eq!(app.status.text(false), "unknown command: /frobnicate");
    }

    #[tokio::test]
    async fn slash_command_response_waits_for_layout_fit_check() {
        let mut app = app_with_rooms(Vec::new());

        app.handle_command(Command::Whoami).await;

        assert_eq!(
            app.pending_command_response.as_deref(),
            Some("select a room before using /whoami")
        );
    }

    #[test]
    fn formatted_body_renders_supported_html_styles() {
        let colors = TuiConfig::test_default().colors;
        let event = EventDto {
            content: Some(serde_json::json!({
                "msgtype": "m.text",
                "body": "bold link code",
                "format": "org.matrix.custom.html",
                "formatted_body": "<strong>bold</strong> <a href=\"https://example.com\">link</a> <code>code</code>"
            })),
            body: Some("bold link code".to_owned()),
            ..event_with_id(
                "$message:example.com",
                "m.room.message",
                Some("bold link code"),
                serde_json::json!({ "msgtype": "m.text", "body": "bold link code" }),
            )
        };
        let sender_labels = vec!["@alice:example.com".to_owned()];
        let lines = message_layout(
            &[&event],
            sender_labels.as_slice(),
            None,
            &colors,
            80,
            &HashMap::new(),
            &HashMap::new(),
            &ImageThumbRows::new(),
            &RelationContext::default(),
            MessageDensity::Dense,
            TimeFormat::H24,
            false,
        )
        .lines;

        assert!(lines[1].spans.iter().any(|span| {
            span.content.contains("bold") && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(lines[1].spans.iter().any(|span| {
            span.content.contains("link")
                && span.style.fg == Some(colors.status)
                && span.style.add_modifier.contains(Modifier::UNDERLINED)
        }));
        assert!(lines[1].spans.iter().any(|span| {
            span.content.contains("code") && span.style.fg == Some(colors.input_hint)
        }));
    }

    #[test]
    fn formatted_body_strips_unsupported_html_and_falls_back_when_empty() {
        let colors = TuiConfig::test_default().colors;
        let event = EventDto {
            content: Some(serde_json::json!({
                "msgtype": "m.text",
                "body": "fallback",
                "format": "org.matrix.custom.html",
                "formatted_body": "<script>alert('x')</script>"
            })),
            body: Some("fallback".to_owned()),
            ..event_with_id(
                "$message:example.com",
                "m.room.message",
                Some("fallback"),
                serde_json::json!({ "msgtype": "m.text", "body": "fallback" }),
            )
        };
        let sender_labels = vec!["@alice:example.com".to_owned()];
        let lines = message_layout(
            &[&event],
            sender_labels.as_slice(),
            None,
            &colors,
            80,
            &HashMap::new(),
            &HashMap::new(),
            &ImageThumbRows::new(),
            &RelationContext::default(),
            MessageDensity::Dense,
            TimeFormat::H24,
            false,
        )
        .lines;

        let text = lines[1]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("fallback"));
        assert!(!text.contains("alert"));
    }

    #[test]
    fn image_layout_counts_caption_and_cached_thumbnail_rows_once() {
        let colors = TuiConfig::test_default().colors;
        let event = event_with_id(
            "$image:example.com",
            "m.room.message",
            Some("caption"),
            serde_json::json!({
                "msgtype": "m.image",
                "body": "caption",
                "filename": "photo.jpg",
                "url": "mxc://example.com/photo"
            }),
        );
        let sender_labels = vec!["@alice:example.com".to_owned()];
        let key = (event.account_id, "mxc://example.com/photo".to_owned());
        let layout = message_layout(
            &[&event],
            sender_labels.as_slice(),
            None,
            &colors,
            80,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::from([(key.clone(), 2)]),
            &RelationContext::default(),
            MessageDensity::Dense,
            TimeFormat::H24,
            false,
        );

        assert_eq!(layout.image_body_rows.get(&key), Some(&2));
        assert_eq!(layout.ranges, vec![1..5]);
        assert_eq!(layout.lines.len(), 5);
    }

    #[test]
    fn normal_layout_puts_body_below_sender_header() {
        let colors = TuiConfig::test_default().colors;
        let event = event_with_id(
            "$message:example.com",
            "m.room.message",
            Some("hello"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
        );
        let sender_labels = vec!["@alice:example.com".to_owned()];
        let layout = message_layout(
            &[&event],
            sender_labels.as_slice(),
            None,
            &colors,
            80,
            &HashMap::new(),
            &HashMap::new(),
            &ImageThumbRows::new(),
            &RelationContext::default(),
            MessageDensity::Normal,
            TimeFormat::H24,
            false,
        );

        // Header row carries the sender but no body; the body is a separate row.
        let header: String = layout.lines[1]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(header.contains("@alice:example.com"));
        assert!(!header.contains("hello"));

        let body: String = layout.lines[2]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        // Body indents to align under the sender (marker "  " + "HH:MM:SS ").
        assert_eq!(body, format!("{}hello", " ".repeat(11)));
        assert_eq!(layout.ranges, vec![1..3]);
    }

    #[test]
    fn normal_layout_image_body_rows_includes_header_row() {
        let colors = TuiConfig::test_default().colors;
        let event = event_with_id(
            "$image:example.com",
            "m.room.message",
            Some("caption"),
            serde_json::json!({
                "msgtype": "m.image",
                "body": "caption",
                "filename": "photo.jpg",
                "url": "mxc://example.com/photo"
            }),
        );
        let sender_labels = vec!["@alice:example.com".to_owned()];
        let key = (event.account_id, "mxc://example.com/photo".to_owned());
        let layout = message_layout(
            &[&event],
            sender_labels.as_slice(),
            None,
            &colors,
            80,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::from([(key.clone(), 2)]),
            &RelationContext::default(),
            MessageDensity::Normal,
            TimeFormat::H24,
            false,
        );

        // Same 2 caption rows + 2 thumbnail rows as the dense case, but the
        // thumbnail offset now includes the separate sender header line.
        assert_eq!(layout.image_body_rows.get(&key), Some(&3));
        assert_eq!(layout.ranges, vec![1..6]);
        assert_eq!(layout.lines.len(), 6);
    }

    #[test]
    fn image_reply_offsets_thumbnail_below_the_reply_line() {
        let colors = TuiConfig::test_default().colors;
        let mut event = event_with_id(
            "$image:example.com",
            "m.room.message",
            Some("caption"),
            serde_json::json!({
                "msgtype": "m.image",
                "body": "caption",
                "filename": "photo.jpg",
                "url": "mxc://example.com/photo"
            }),
        );
        // Mark the image as a reply: a reply-context line renders between the
        // header and the body, so the thumbnail must drop below it.
        event.relates_to = Some(serde_json::json!({
            "m.in_reply_to": { "event_id": "$parent:example.com" }
        }));
        let sender_labels = vec!["@alice:example.com".to_owned()];
        let key = (event.account_id, "mxc://example.com/photo".to_owned());
        let layout = message_layout(
            &[&event],
            sender_labels.as_slice(),
            None,
            &colors,
            80,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::from([(key.clone(), 2)]),
            &RelationContext::default(),
            MessageDensity::Normal,
            TimeFormat::H24,
            false,
        );

        // header(1) + reply line(1) + caption(2) = 4 rows above the thumbnail.
        // Before the fix this was 3, so the thumbnail overwrote the filename.
        assert_eq!(layout.image_body_rows.get(&key), Some(&4));
        // 4 rows + 2 thumbnail rows = 6 message rows after the date separator.
        assert_eq!(layout.ranges, vec![1..7]);
        assert_eq!(layout.lines.len(), 7);
    }

    #[test]
    fn message_navigation_uses_rendered_image_ranges() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.page_size = 3;
        app.messages.scroll = 0;
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![
                event_with_id(
                    "$image:example.com",
                    "m.room.message",
                    Some("caption"),
                    serde_json::json!({
                        "msgtype": "m.image",
                        "body": "caption",
                        "filename": "photo.jpg",
                        "url": "mxc://example.com/photo"
                    }),
                ),
                event_with_id(
                    "$next:example.com",
                    "m.room.message",
                    Some("next"),
                    serde_json::json!({ "msgtype": "m.text", "body": "next" }),
                ),
            ],
        );
        app.set_message_layout(
            vec![
                "$image:example.com".to_owned(),
                "$next:example.com".to_owned(),
            ],
            vec![0..4, 4..5],
        );

        app.messages.selection = Some("$next:example.com".to_owned());
        app.ensure_message_index_visible(1);

        assert_eq!(app.messages.scroll, 2);
    }

    #[test]
    fn message_navigation_selects_displayed_messages() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![
                event_with_id(
                    "$one:example.com",
                    "m.room.message",
                    Some("one"),
                    serde_json::json!({ "msgtype": "m.text", "body": "one" }),
                ),
                event_with_id(
                    "$two:example.com",
                    "m.room.message",
                    Some("two"),
                    serde_json::json!({ "msgtype": "m.text", "body": "two" }),
                ),
            ],
        );

        app.move_selected_message(1);
        assert_eq!(app.selected_message_id(), Some("$one:example.com"));
        app.move_selected_message(1);
        assert_eq!(app.selected_message_id(), Some("$two:example.com"));
        app.move_selected_message(-1);
        assert_eq!(app.selected_message_id(), Some("$one:example.com"));
    }

    #[test]
    fn message_navigation_clamps_at_list_edges() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![
                event_with_id(
                    "$one:example.com",
                    "m.room.message",
                    Some("one"),
                    serde_json::json!({ "msgtype": "m.text", "body": "one" }),
                ),
                event_with_id(
                    "$two:example.com",
                    "m.room.message",
                    Some("two"),
                    serde_json::json!({ "msgtype": "m.text", "body": "two" }),
                ),
            ],
        );

        app.move_selected_message(-1);
        assert_eq!(app.selected_message_id(), Some("$two:example.com"));
        app.move_selected_message(1);
        assert_eq!(app.selected_message_id(), Some("$two:example.com"));
        app.move_selected_message(-10);
        assert_eq!(app.selected_message_id(), Some("$one:example.com"));
    }

    #[test]
    fn message_navigation_moves_by_message_not_display_line() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        // Pin dense layout so the asserted scroll offsets match its line math.
        app.display.message_density = MessageDensity::Dense;
        app.messages.page_size = 2;
        app.messages.width = 80;
        app.messages.scroll = 0;
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![
                event_with_id(
                    "$multi:example.com",
                    "m.room.message",
                    Some("one\ntwo\nthree"),
                    serde_json::json!({ "msgtype": "m.text", "body": "one\ntwo\nthree" }),
                ),
                event_with_id(
                    "$next:example.com",
                    "m.room.message",
                    Some("next"),
                    serde_json::json!({ "msgtype": "m.text", "body": "next" }),
                ),
            ],
        );

        app.move_selected_message(1);
        assert_eq!(app.selected_message_id(), Some("$multi:example.com"));
        app.move_selected_message(1);
        assert_eq!(app.selected_message_id(), Some("$next:example.com"));
        assert_eq!(app.messages.scroll, 3);
        app.move_selected_message(-1);
        assert_eq!(app.selected_message_id(), Some("$multi:example.com"));
        assert_eq!(app.messages.scroll, 1);
    }

    #[test]
    fn message_page_navigation_uses_message_pane_page_size() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        // Pin dense layout so the asserted scroll offsets match its line math.
        app.display.message_density = MessageDensity::Dense;
        app.messages.page_size = 3;
        app.messages.scroll = 0;
        app.messages.events.insert(
            RoomKey::from(&room),
            (0..8)
                .map(|index| {
                    event_with_id(
                        &format!("${index}:example.com"),
                        "m.room.message",
                        Some("message"),
                        serde_json::json!({ "msgtype": "m.text", "body": "message" }),
                    )
                })
                .collect(),
        );

        app.page_selected_message(1);
        assert_eq!(app.selected_message_id(), Some("$3:example.com"));
        assert_eq!(app.messages.scroll, 4);
        app.page_selected_message(-1);
        assert_eq!(app.selected_message_id(), Some("$0:example.com"));
        assert_eq!(app.messages.scroll, 1);
    }

    #[test]
    fn message_navigation_ignores_hidden_state_events() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![
                event_with_state_key(
                    "$topic:example.com",
                    "m.room.topic",
                    Some(""),
                    None,
                    serde_json::json!({ "topic": "new topic" }),
                ),
                event_with_id(
                    "$message:example.com",
                    "m.room.message",
                    Some("message"),
                    serde_json::json!({ "msgtype": "m.text", "body": "message" }),
                ),
            ],
        );

        app.move_selected_message(1);

        assert_eq!(app.selected_message_id(), Some("$message:example.com"));
    }

    #[tokio::test]
    async fn reply_and_thread_actions_target_selected_message() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![event_with_id(
                "$message:example.com",
                "m.room.message",
                Some("message"),
                serde_json::json!({ "msgtype": "m.text", "body": "message" }),
            )],
        );
        app.messages.selection = Some("$message:example.com".to_owned());

        app.start_reply_to_selected_message();
        assert_eq!(app.pending_reply.as_deref(), Some("$message:example.com"));
        assert_eq!(app.pending_thread, None);

        // A standalone message heads no thread, so /thread composes a new thread
        // rooted at it (ADR 0032 M4) rather than opening a panel.
        app.start_thread_from_selected_message().await;
        assert_eq!(app.pending_thread.as_deref(), Some("$message:example.com"));
        assert_eq!(app.pending_reply, None);
    }

    fn reply_event(event_id: &str, target: &str) -> EventDto {
        let mut event = event_with_id(
            event_id,
            "m.room.message",
            Some("reply body"),
            serde_json::json!({ "msgtype": "m.text", "body": "reply body" }),
        );
        event.relates_to = Some(serde_json::json!({
            "m.in_reply_to": { "event_id": target }
        }));
        event
    }

    fn thread_event(event_id: &str, root: &str, body: &str) -> EventDto {
        let mut event = event_with_id(
            event_id,
            "m.room.message",
            Some(body),
            serde_json::json!({ "msgtype": "m.text", "body": body }),
        );
        event.relates_to = Some(serde_json::json!({
            "rel_type": "m.thread",
            "event_id": root
        }));
        event
    }

    fn ids(events: &[&EventDto]) -> Vec<String> {
        events.iter().map(|event| event.event_id.clone()).collect()
    }

    fn unread_thread(root: &str, count: usize, latest_ts: i64) -> UnreadThread {
        UnreadThread {
            root_event_id: root.to_owned(),
            unread_count: count,
            latest_event_id: format!("{root}-reply"),
            latest_sender: "@bob:example.com".to_owned(),
            latest_body: "new reply".to_owned(),
            latest_ts,
            recent: vec![UnreadThreadPreview {
                event_id: format!("{root}-reply"),
                sender: "@bob:example.com".to_owned(),
                body: "new reply".to_owned(),
                origin_ts: latest_ts,
            }],
            counted: std::collections::HashSet::from([format!("{root}-reply")]),
        }
    }

    #[test]
    fn reply_context_resolves_from_loaded_slice() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        let original = event_with_id(
            "$orig:example.com",
            "m.room.message",
            Some("hello world"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello world" }),
        );
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![
                original,
                reply_event("$reply:example.com", "$orig:example.com"),
            ],
        );

        let events = app.selected_events();
        let ctx = app.relation_context(&events);
        let preview = ctx.replies.get("$reply:example.com").expect("preview");
        assert_eq!(preview.sender, "@alice:example.com");
        assert_eq!(preview.snippet, "hello world");
    }

    #[test]
    fn reply_context_absent_when_target_off_slice() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![reply_event("$reply:example.com", "$missing:example.com")],
        );

        let events = app.selected_events();
        let ctx = app.relation_context(&events);
        // No resolved preview => the layout renders the placeholder line.
        assert!(ctx.replies.is_empty());
    }

    #[test]
    fn thread_members_are_hidden_from_main_timeline_and_badged_on_root() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        let root = event_with_id(
            "$root:example.com",
            "m.room.message",
            Some("Let's discuss"),
            serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
        );
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![
                root,
                thread_event("$m1:example.com", "$root:example.com", "first"),
                thread_event("$m2:example.com", "$root:example.com", "second"),
            ],
        );

        // Main timeline shows only the root.
        assert_eq!(ids(&app.selected_events()), vec!["$root:example.com"]);

        let events = app.selected_events();
        let ctx = app.relation_context(&events);
        let badge = ctx.thread_badges.get("$root:example.com").expect("badge");
        assert_eq!(badge.count, 2);
        assert_eq!(badge.latest_sender.as_deref(), Some("@alice:example.com"));
        assert_eq!(badge.latest_snippet.as_deref(), Some("second"));
    }

    #[test]
    fn thread_badge_count_prefers_server_aggregate() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        let root = event_with_id(
            "$root:example.com",
            "m.room.message",
            Some("Let's discuss"),
            serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
        );
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![
                root,
                thread_event("$m1:example.com", "$root:example.com", "first"),
            ],
        );
        app.thread_summaries.insert(
            RoomKey::from(&room),
            HashMap::from([(
                "$root:example.com".to_owned(),
                crate::api::ThreadSummaryDto {
                    root_event_id: "$root:example.com".to_owned(),
                    reply_count: 7,
                },
            )]),
        );

        let events = app.selected_events();
        let ctx = app.relation_context(&events);
        // Seven total on the server even though only one member is in the slice.
        assert_eq!(ctx.thread_badges.get("$root:example.com").unwrap().count, 7);
    }

    #[test]
    fn stale_relation_outcome_cannot_replace_newer_thread_summary() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        let key = RoomKey::from(&room);
        app.relation_refresh_latest.insert(key.clone(), 2);

        app.apply_relation_outcome(relations::RelationOutcome {
            room_key: key.clone(),
            refresh_id: 1,
            account_id: room.account_id,
            threads: Some(HashMap::from([(
                "$root:example.com".to_owned(),
                crate::api::ThreadSummaryDto {
                    root_event_id: "$root:example.com".to_owned(),
                    reply_count: 99,
                },
            )])),
            replies: Vec::new(),
            is_incremental: false,
        });

        assert!(!app.thread_summaries.contains_key(&key));

        app.apply_relation_outcome(relations::RelationOutcome {
            room_key: key.clone(),
            refresh_id: 2,
            account_id: room.account_id,
            threads: Some(HashMap::from([(
                "$root:example.com".to_owned(),
                crate::api::ThreadSummaryDto {
                    root_event_id: "$root:example.com".to_owned(),
                    reply_count: 3,
                },
            )])),
            replies: Vec::new(),
            is_incremental: false,
        });

        assert_eq!(
            app.thread_summaries[&key]["$root:example.com"].reply_count,
            3
        );
    }

    #[test]
    fn live_thread_member_increments_cached_server_summary() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        let key = RoomKey::from(&room);
        let root = event_with_id(
            "$root:example.com",
            "m.room.message",
            Some("Let's discuss"),
            serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
        );
        app.messages.events.insert(key.clone(), vec![root]);
        app.thread_summaries.insert(
            key.clone(),
            HashMap::from([(
                "$root:example.com".to_owned(),
                crate::api::ThreadSummaryDto {
                    root_event_id: "$root:example.com".to_owned(),
                    reply_count: 2,
                },
            )]),
        );

        let live = thread_event("$m3:example.com", "$root:example.com", "third");
        let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(live)));

        assert_eq!(action, LiveFrameAction::None);
        assert_eq!(
            app.thread_summaries[&key]["$root:example.com"].reply_count,
            3
        );
        let events = app.selected_events();
        let ctx = app.relation_context(&events);
        assert_eq!(ctx.thread_badges.get("$root:example.com").unwrap().count, 3);
    }

    #[test]
    fn live_thread_member_marks_thread_unread_when_panel_closed() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        let key = RoomKey::from(&room);
        let root = event_with_id(
            "$root:example.com",
            "m.room.message",
            Some("Let's discuss"),
            serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
        );
        app.messages.events.insert(key.clone(), vec![root]);

        let mut live = thread_event("$reply:example.com", "$root:example.com", "new reply");
        live.sender = "@bob:example.com".to_owned();
        app.handle_live_frame(LiveFrame::Timeline(Box::new(live)));

        assert_eq!(
            app.unread_threads[&key]["$root:example.com"].unread_count,
            1
        );
        let events = app.selected_events();
        let ctx = app.relation_context(&events);
        assert_eq!(ctx.thread_badges["$root:example.com"].unread_count, 1);
        assert_eq!(app.rooms.unread.get(&key), None);
    }

    #[test]
    fn live_thread_member_for_unselected_room_marks_room_and_thread_unread() {
        let unread_room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let other = room(
            "!other:example.com",
            Some("#other:example.com"),
            Some("Other"),
        );
        let mut app = app_with_rooms(vec![unread_room.clone(), other]);
        app.rooms.selected = Some(1);
        let key = RoomKey::from(&unread_room);

        let mut live = thread_event("$reply:example.com", "$root:example.com", "new reply");
        live.sender = "@bob:example.com".to_owned();
        app.handle_live_frame(LiveFrame::Timeline(Box::new(live)));

        assert_eq!(app.rooms.unread.get(&key).copied(), Some(1));
        assert_eq!(
            app.unread_threads[&key]["$root:example.com"].unread_count,
            1
        );
    }

    #[test]
    fn own_live_thread_member_does_not_mark_thread_unread() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        let key = RoomKey::from(&room);
        let root = event_with_id(
            "$root:example.com",
            "m.room.message",
            Some("Let's discuss"),
            serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
        );
        app.messages.events.insert(key.clone(), vec![root]);
        app.seed_own_senders_from_rooms();

        let live = thread_event("$reply:example.com", "$root:example.com", "my reply");
        app.handle_live_frame(LiveFrame::Timeline(Box::new(live)));

        assert!(!app.unread_threads.contains_key(&key));
    }

    #[test]
    fn clearing_thread_unread_removes_only_that_thread() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        let key = RoomKey::from(&room);
        app.unread_threads.insert(
            key.clone(),
            HashMap::from([
                (
                    "$root:example.com".to_owned(),
                    unread_thread("$root:example.com", 2, 2),
                ),
                (
                    "$other-root:example.com".to_owned(),
                    unread_thread("$other-root:example.com", 1, 1),
                ),
            ]),
        );

        app.clear_unread_thread(&key, "$root:example.com");

        assert!(!app.unread_threads[&key].contains_key("$root:example.com"));
        assert!(app.unread_threads[&key].contains_key("$other-root:example.com"));
    }

    #[test]
    fn unread_thread_entries_sort_newest_first_and_include_context() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        let key = RoomKey::from(&room);
        app.messages.events.insert(
            key.clone(),
            vec![event_with_id(
                "$root:example.com",
                "m.room.message",
                Some("Root topic"),
                serde_json::json!({ "msgtype": "m.text", "body": "Root topic" }),
            )],
        );
        app.unread_threads.insert(
            key,
            HashMap::from([(
                "$root:example.com".to_owned(),
                unread_thread("$root:example.com", 2, 2),
            )]),
        );

        let entries = app.unread_thread_entries();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].room_title, "Room");
        assert_eq!(entries[0].root_snippet.as_deref(), Some("Root topic"));
        assert_eq!(entries[0].unread_count, 2);
        assert_eq!(entries[0].recent.len(), 1);
    }

    #[test]
    fn unread_thread_previews_keep_three_newest_posts() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        let key = RoomKey::from(&room);

        for idx in 0..5 {
            let mut event = thread_event(
                &format!("$reply{idx}:example.com"),
                "$root:example.com",
                &format!("reply {idx}"),
            );
            event.sender = format!("@sender{idx}:example.com");
            event.origin_ts = idx;
            app.mark_thread_unread_from_event(&key, "$root:example.com", &event);
        }

        let thread = &app.unread_threads[&key]["$root:example.com"];

        assert_eq!(thread.unread_count, 5);
        assert_eq!(
            thread
                .recent
                .iter()
                .map(|preview| preview.body.as_str())
                .collect::<Vec<_>>(),
            vec!["reply 4", "reply 3", "reply 2"]
        );
    }

    #[test]
    fn unread_threads_command_opens_picker_when_entries_exist() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        let key = RoomKey::from(&room);
        app.unread_threads.insert(
            key,
            HashMap::from([(
                "$root:example.com".to_owned(),
                unread_thread("$root:example.com", 1, 1),
            )]),
        );

        app.open_unread_threads_picker();

        assert_eq!(app.mode, Mode::Popup(PopupKind::UnreadThreads));
        assert_eq!(app.unread_thread_selection, 0);
    }

    #[test]
    fn unread_thread_selection_follows_identity_after_resort() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        let key = RoomKey::from(&room);
        app.unread_threads.insert(
            key.clone(),
            HashMap::from([
                (
                    "$root-a:example.com".to_owned(),
                    unread_thread("$root-a:example.com", 1, 20),
                ),
                (
                    "$root-b:example.com".to_owned(),
                    unread_thread("$root-b:example.com", 1, 10),
                ),
            ]),
        );

        app.open_unread_threads_picker();
        assert_eq!(app.unread_thread_selection, 0);
        assert_eq!(
            app.unread_thread_selected
                .as_ref()
                .map(|selected| selected.root_event_id.as_str()),
            Some("$root-a:example.com")
        );

        let mut newer = thread_event("$reply-b:example.com", "$root-b:example.com", "newer");
        newer.sender = "@bob:example.com".to_owned();
        newer.origin_ts = 30;
        app.mark_thread_unread_from_event(&key, "$root-b:example.com", &newer);
        let entries = app.unread_thread_entries();
        app.sync_unread_thread_selection(&entries);

        assert_eq!(entries[0].root_event_id, "$root-b:example.com");
        assert_eq!(app.unread_thread_selection, 1);
        assert_eq!(
            app.unread_thread_selected
                .as_ref()
                .map(|selected| selected.root_event_id.as_str()),
            Some("$root-a:example.com")
        );
    }

    #[test]
    fn unread_threads_command_reports_empty_state_without_popup() {
        let mut app = app_with_rooms(Vec::new());

        app.open_unread_threads_picker();

        assert_eq!(app.mode, Mode::Compose);
        assert_eq!(app.status.text(false), "no unread threads");
    }

    #[test]
    fn live_thread_member_promoted_to_main_timeline_when_panel_closed() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        let key = RoomKey::from(&room);
        let root = event_with_id(
            "$root:example.com",
            "m.room.message",
            Some("Let's discuss"),
            serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
        );
        app.messages.events.insert(key.clone(), vec![root]);

        // Live thread member arrives with no thread panel open.
        let live = thread_event("$reply:example.com", "$root:example.com", "my reply");
        app.handle_live_frame(LiveFrame::Timeline(Box::new(live)));

        // The promoted set must contain the new event.
        assert!(app.promoted_thread_events.contains("$reply:example.com"));

        // selected_events() must surface the promoted event in the main timeline.
        let events = app.selected_events();
        let ids = ids(&events);
        assert!(
            ids.contains(&"$reply:example.com".to_owned()),
            "promoted thread member should appear in main timeline"
        );

        // The relation context must carry a thread_context entry for the member.
        let ctx = app.relation_context(&events);
        assert!(
            ctx.thread_contexts.contains_key("$reply:example.com"),
            "thread context should be built for the promoted event"
        );
        // Root is in the slice, so the context resolves (Some(preview)).
        assert!(
            ctx.thread_contexts["$reply:example.com"].is_some(),
            "thread context should resolve when root is in the slice"
        );
    }

    #[test]
    fn promoted_events_cleared_when_thread_panel_opens() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        let key = RoomKey::from(&room);
        let root = event_with_id(
            "$root:example.com",
            "m.room.message",
            Some("Let's discuss"),
            serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
        );
        app.messages.events.insert(key.clone(), vec![root]);

        // Promote a live reply.
        let live = thread_event("$reply:example.com", "$root:example.com", "my reply");
        app.handle_live_frame(LiveFrame::Timeline(Box::new(live)));
        assert!(app.promoted_thread_events.contains("$reply:example.com"));

        // Opening the thread panel for that root should clear the promotion.
        app.promoted_thread_events.retain(|id| {
            app.messages.events.get(&key).is_none_or(|events| {
                events
                    .iter()
                    .find(|e| &e.event_id == id)
                    .and_then(|e| e.thread_relation())
                    != Some("$root:example.com")
            })
        });
        assert!(
            !app.promoted_thread_events.contains("$reply:example.com"),
            "promoted event should be cleared when panel opens for its root"
        );
    }

    #[test]
    fn thread_panel_shows_root_and_members_then_closes() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        let root = event_with_id(
            "$root:example.com",
            "m.room.message",
            Some("Let's discuss"),
            serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
        );
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![
                root,
                thread_event("$m1:example.com", "$root:example.com", "first"),
                thread_event("$m2:example.com", "$root:example.com", "second"),
            ],
        );

        app.thread_panel = Some("$root:example.com".to_owned());
        assert_eq!(
            ids(&app.selected_events()),
            vec!["$root:example.com", "$m1:example.com", "$m2:example.com"]
        );

        // Inside the panel the root is labeled and no badge clutters the view.
        let events = app.selected_events();
        let ctx = app.relation_context(&events);
        assert_eq!(ctx.thread_root.as_deref(), Some("$root:example.com"));
        assert!(ctx.thread_badges.is_empty());

        assert!(app.close_thread_panel());
        assert!(app.thread_panel.is_none());
        // After closing the panel, the thread root message should be selected.
        assert_eq!(app.messages.selection.as_deref(), Some("$root:example.com"));
        // Idempotent: a second Esc in the main timeline is not consumed here.
        assert!(!app.close_thread_panel());
    }

    #[test]
    fn search_jump_merges_fetched_thread_before_opening_panel() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);

        let mut root = event_with_id(
            "$root:example.com",
            "m.room.message",
            Some("Let's discuss"),
            serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
        );
        root.origin_ts = 100;
        let mut hit = thread_event("$hit:example.com", "$root:example.com", "search hit");
        hit.origin_ts = 200;
        let mut fetched_member =
            thread_event("$fetched:example.com", "$root:example.com", "older context");
        fetched_member.origin_ts = 300;

        app.handle_search_outcome(SearchOutcome::Jump {
            hit: hit.clone(),
            action: SearchJumpAction::View,
            room_refresh: None,
            result: Ok(TimelinePage {
                events: vec![hit],
                next_cursor: None,
            }),
            thread_load: Some(Box::new(SearchJumpThreadLoad {
                timeline: Ok(TimelinePage {
                    events: vec![fetched_member],
                    next_cursor: None,
                }),
                root_event: Ok(root),
            })),
        });

        assert_eq!(app.thread_panel.as_deref(), Some("$root:example.com"));
        assert_eq!(app.messages.selection.as_deref(), Some("$hit:example.com"));
        assert_eq!(
            ids(&app.selected_events()),
            vec![
                "$root:example.com",
                "$hit:example.com",
                "$fetched:example.com"
            ]
        );
    }

    #[test]
    fn selecting_a_thread_root_hints_the_open_thread_shortcut() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        let root = event_with_id(
            "$root:example.com",
            "m.room.message",
            Some("Let's discuss"),
            serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
        );
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![
                root,
                thread_event("$m1:example.com", "$root:example.com", "first"),
            ],
        );

        app.move_selected_message(1);
        assert_eq!(app.selected_message_id(), Some("$root:example.com"));
        assert!(
            app.status.text(false).contains("open thread"),
            "status should hint the thread shortcut: {:?}",
            app.status.text(false)
        );
    }

    #[test]
    fn is_thread_root_detects_members_and_server_summary() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        let plain = event_with_id(
            "$plain:example.com",
            "m.room.message",
            Some("nothing"),
            serde_json::json!({ "msgtype": "m.text", "body": "nothing" }),
        );
        let root = event_with_id(
            "$root:example.com",
            "m.room.message",
            Some("root"),
            serde_json::json!({ "msgtype": "m.text", "body": "root" }),
        );
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![
                plain,
                root,
                thread_event("$m1:example.com", "$root:example.com", "first"),
            ],
        );

        assert!(app.is_thread_root("$root:example.com"));
        assert!(!app.is_thread_root("$plain:example.com"));
    }

    #[tokio::test]
    async fn message_action_commands_target_most_recent_message_without_selection() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![
                event_with_id(
                    "$older:example.com",
                    "m.room.message",
                    Some("older"),
                    serde_json::json!({ "msgtype": "m.text", "body": "older" }),
                ),
                event_with_id(
                    "$newest:example.com",
                    "m.room.message",
                    Some("newest"),
                    serde_json::json!({ "msgtype": "m.text", "body": "newest" }),
                ),
            ],
        );

        app.handle_command(Command::React(None)).await;
        assert_eq!(app.selected_message_id(), Some("$newest:example.com"));
        assert_eq!(
            app.mode,
            Mode::Reacting {
                event_id: "$newest:example.com".to_owned()
            }
        );

        app.mode = Mode::Compose;
        app.messages.selection = None;
        app.handle_command(Command::Reply).await;
        assert_eq!(app.selected_message_id(), Some("$newest:example.com"));
        assert_eq!(app.pending_reply.as_deref(), Some("$newest:example.com"));

        app.messages.selection = None;
        app.handle_command(Command::Thread).await;
        assert_eq!(app.selected_message_id(), Some("$newest:example.com"));
        assert_eq!(app.pending_thread.as_deref(), Some("$newest:example.com"));
    }

    #[tokio::test]
    async fn message_action_commands_preserve_an_existing_selection() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![
                event_with_id(
                    "$selected:example.com",
                    "m.room.message",
                    Some("selected"),
                    serde_json::json!({ "msgtype": "m.text", "body": "selected" }),
                ),
                event_with_id(
                    "$newest:example.com",
                    "m.room.message",
                    Some("newest"),
                    serde_json::json!({ "msgtype": "m.text", "body": "newest" }),
                ),
            ],
        );
        app.messages.selection = Some("$selected:example.com".to_owned());

        app.handle_command(Command::React(None)).await;

        assert_eq!(app.selected_message_id(), Some("$selected:example.com"));
        assert_eq!(
            app.mode,
            Mode::Reacting {
                event_id: "$selected:example.com".to_owned()
            }
        );
    }

    #[test]
    fn entry_status_hides_event_codes_unless_debug_is_enabled() {
        let mut app = app_with_rooms(Vec::new());
        app.status = Status::EventAction {
            debug: "editing $message:example.com - Esc to cancel".to_owned(),
            redacted: "editing message - Esc to cancel",
        };

        assert_eq!(entry_status_text(&app), "editing message - Esc to cancel");

        app.display.debug = true;

        assert_eq!(
            entry_status_text(&app),
            "editing $message:example.com - Esc to cancel"
        );
    }

    #[test]
    fn entry_status_hides_live_socket_status_unless_debug_is_enabled() {
        let mut app = app_with_rooms(Vec::new());
        app.status = Status::Debug("live WebSocket connected".to_owned());

        assert_eq!(entry_status_text(&app), "");

        app.display.debug = true;

        assert_eq!(entry_status_text(&app), "live WebSocket connected");
    }

    #[test]
    fn reconnecting_live_socket_status_is_visible() {
        let mut app = app_with_rooms(Vec::new());

        let action = app.handle_live_frame(LiveFrame::Reconnecting {
            reason: "connection reset".to_owned(),
            delay: std::time::Duration::from_secs(4),
        });

        assert_eq!(action, LiveFrameAction::None);
        assert_eq!(
            entry_status_text(&app),
            "live WebSocket reconnecting in 4s: connection reset"
        );
    }

    #[test]
    fn in_flight_lifecycle_status_ignores_live_socket_updates() {
        let mut app = app_with_rooms(Vec::new());
        app.lifecycle_busy = true;
        app.status = Status::Info("logging in @alice:example.com…".to_owned());

        let action = app.handle_live_frame(LiveFrame::Reconnecting {
            reason: "connection reset".to_owned(),
            delay: std::time::Duration::from_secs(4),
        });

        assert_eq!(action, LiveFrameAction::None);
        assert_eq!(app.status.text(false), "logging in @alice:example.com…");
    }

    #[test]
    fn lifecycle_prompt_status_survives_background_room_refresh() {
        let mut app = app_with_rooms(Vec::new());
        app.mode = Mode::LoginUsername;
        app.status = Status::Info("Matrix ID: @user:example.com".to_owned());

        app.apply_room_refresh(Vec::new());

        assert_eq!(app.status.text(false), "Matrix ID: @user:example.com");
    }

    #[test]
    fn in_flight_lifecycle_status_survives_background_room_refresh() {
        let mut app = app_with_rooms(Vec::new());
        app.lifecycle_busy = true;
        app.status = Status::Info("deleting @alice:example.com…".to_owned());

        app.apply_room_refresh(Vec::new());

        assert_eq!(app.status.text(false), "deleting @alice:example.com…");
    }

    #[tokio::test]
    async fn clear_input_shortcut_aborts_message_selection() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![event_with_id(
                "$message:example.com",
                "m.room.message",
                Some("message"),
                serde_json::json!({ "msgtype": "m.text", "body": "message" }),
            )],
        );
        app.messages.selection = Some("$message:example.com".to_owned());
        app.input.buffer = "/room room".to_owned();
        app.input.cursor = app.input.buffer.len();

        app.handle_key(KeyEvent::from(KeyCode::Esc)).await;

        assert_eq!(app.selected_message_id(), None);
        assert_eq!(app.input.buffer, "");
        assert_eq!(app.input.cursor, 0);
    }

    #[tokio::test]
    async fn input_cursor_supports_readline_start_and_end() {
        let mut app = app_with_rooms(Vec::new());
        for ch in "abc".chars() {
            app.handle_key(KeyEvent::from(KeyCode::Char(ch))).await;
        }

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
            .await;
        app.handle_key(KeyEvent::from(KeyCode::Char('X'))).await;
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
            .await;
        app.handle_key(KeyEvent::from(KeyCode::Char('Y'))).await;

        assert_eq!(app.input.buffer, "XabcY");
        assert_eq!(app.input.cursor, app.input.buffer.len());
    }

    #[tokio::test]
    async fn arrow_up_from_compose_enters_message_list_mode() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![
                event_with_id(
                    "$one:example.com",
                    "m.room.message",
                    Some("first"),
                    serde_json::json!({ "msgtype": "m.text", "body": "first" }),
                ),
                event_with_id(
                    "$two:example.com",
                    "m.room.message",
                    Some("second"),
                    serde_json::json!({ "msgtype": "m.text", "body": "second" }),
                ),
            ],
        );

        // Up from no selection: select the last message; switches to MessageList, input untouched
        app.handle_key(KeyEvent::from(KeyCode::Up)).await;
        assert_eq!(app.input.buffer, "");
        assert_eq!(app.selected_message_id(), Some("$two:example.com"));
        assert!(matches!(app.mode, Mode::MessageList));

        // Up again (now in MessageList): move to the previous message
        app.handle_key(KeyEvent::from(KeyCode::Up)).await;
        assert_eq!(app.input.buffer, "");
        assert_eq!(app.selected_message_id(), Some("$one:example.com"));
        assert!(matches!(app.mode, Mode::MessageList));

        // Up at the first message: stay put
        app.handle_key(KeyEvent::from(KeyCode::Up)).await;
        assert_eq!(app.selected_message_id(), Some("$one:example.com"));

        // Down: move forward
        app.handle_key(KeyEvent::from(KeyCode::Down)).await;
        assert_eq!(app.selected_message_id(), Some("$two:example.com"));
        assert!(matches!(app.mode, Mode::MessageList));

        // Down at the last message: stay put (Esc returns to Compose)
        app.handle_key(KeyEvent::from(KeyCode::Down)).await;
        assert_eq!(app.selected_message_id(), Some("$two:example.com"));
        assert!(matches!(app.mode, Mode::MessageList));
    }

    #[tokio::test]
    async fn arrow_up_with_no_messages_enters_message_list() {
        let mut app = app_with_rooms(Vec::new());
        app.handle_key(KeyEvent::from(KeyCode::Up)).await;
        assert_eq!(app.input.buffer, "");
        assert!(matches!(app.mode, Mode::MessageList));
    }

    #[tokio::test]
    async fn media_preview_hotkey_opens_popup_for_selected_image() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.selection = Some("$image:example.com".to_owned());
        app.mode = Mode::MessageList;
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![event_with_id(
                "$image:example.com",
                "m.room.message",
                Some("photo.jpg"),
                serde_json::json!({
                    "msgtype": "m.image",
                    "body": "photo.jpg",
                    "url": "mxc://example.com/photo"
                }),
            )],
        );

        app.handle_key(KeyEvent::from(KeyCode::Char('v'))).await;

        assert_eq!(app.mode, Mode::Popup(PopupKind::MediaPreview));
    }

    fn ready_image() -> Arc<image::DynamicImage> {
        Arc::new(image::DynamicImage::new_rgb8(1, 1))
    }

    /// #51: the outcomes of a protocol request are named, and the two that are
    /// faults are counted rather than dropped in silence.
    #[test]
    fn request_protocol_reports_why_it_did_not_start_an_encode() {
        let mut app = app_with_rooms(Vec::new());
        let key = MediaKey::new(Uuid::nil(), "mxc://example.com/a".to_owned());

        // Degenerate geometry: nothing to encode into.
        assert_eq!(
            app.request_protocol(key.clone(), Size::new(0, 4)),
            ProtocolRequest::EmptySize
        );

        // The image has not been decoded yet. Expected, self-correcting, and
        // deliberately not counted as a drop.
        assert_eq!(
            app.request_protocol(key.clone(), Size::new(8, 4)),
            ProtocolRequest::ImageNotReady
        );
        assert_eq!(app.protocol_drops, ProtocolDropCounts::default());

        // Image ready but the media channel was never wired: every encode for
        // the life of the process dies here, so it is counted.
        app.image_cache
            .insert(key.clone(), ImageState::Ready(ready_image()));
        assert_eq!(
            app.request_protocol(key.clone(), Size::new(8, 4)),
            ProtocolRequest::ChannelUnwired
        );
        assert_eq!(app.protocol_drops.channel_unwired, 1);
        assert_eq!(app.protocol_drops.cache_saturated, 0);
    }

    // `Started` spawns the encode, so this needs a runtime.
    #[tokio::test]
    async fn request_protocol_counts_a_saturated_protocol_cache() {
        let mut app = app_with_rooms(Vec::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(MEDIA_WORKERS * 2);
        app.set_media_sender(tx);

        let key = MediaKey::new(Uuid::nil(), "mxc://example.com/a".to_owned());
        app.image_cache
            .insert(key.clone(), ImageState::Ready(ready_image()));

        // Every slot mid-encode, so `evict_lru_where` can free nothing.
        for i in 0..PROTOCOL_CACHE_LIMIT {
            let filler = ProtocolKey {
                media: MediaKey::new(Uuid::nil(), format!("mxc://example.com/filler{i}")),
                size: Size::new(8, 4),
            };
            app.proto_cache
                .insert(filler.clone(), ProtocolState::Encoding);
            touch_lru(&mut app.proto_cache_order, &filler);
        }

        assert_eq!(
            app.request_protocol(key.clone(), Size::new(8, 4)),
            ProtocolRequest::CacheSaturated
        );
        assert_eq!(app.protocol_drops.cache_saturated, 1);

        // Self-healing: once a slot settles, the same request goes through.
        let settled = ProtocolKey {
            media: MediaKey::new(Uuid::nil(), "mxc://example.com/filler0".to_owned()),
            size: Size::new(8, 4),
        };
        app.proto_cache
            .insert(settled, ProtocolState::Failed("boom".to_owned()));
        assert_eq!(
            app.request_protocol(key, Size::new(8, 4)),
            ProtocolRequest::Started
        );
        assert_eq!(app.protocol_drops.cache_saturated, 1);
    }

    /// #49: the Sixel retransmit state is per-preview, not global.
    ///
    /// The counter selects between two encodings of the same image that differ
    /// only in a trailing SGR, so a preview inheriting odd parity opens on the
    /// alternate variant; the deadline used to be a main-loop local, so an
    /// interval that elapsed while nothing was open left the next preview due
    /// for a retransmit on its very first tick.
    #[test]
    fn opening_a_media_preview_resets_the_sixel_retransmit_state() {
        let mut app = app_with_rooms(Vec::new());
        app.sixel_preview_generation = 7;
        app.sixel_preview_refresh_after = Instant::now() - Duration::from_secs(60);

        app.open_popup(PopupKind::MediaPreview);

        assert_eq!(app.sixel_preview_generation, 0);
        assert!(app.sixel_preview_refresh_after > Instant::now());
    }

    /// Only the media preview owns this state; other popups leave it alone.
    #[test]
    fn opening_another_popup_leaves_the_sixel_retransmit_state_alone() {
        let mut app = app_with_rooms(Vec::new());
        app.sixel_preview_generation = 7;
        let deadline = Instant::now() - Duration::from_secs(60);
        app.sixel_preview_refresh_after = deadline;

        app.open_popup(PopupKind::Help);

        assert_eq!(app.sixel_preview_generation, 7);
        assert_eq!(app.sixel_preview_refresh_after, deadline);
    }

    #[tokio::test]
    async fn global_message_navigation_abandons_edit_mode() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![
                event_with_id(
                    "$one:example.com",
                    "m.room.message",
                    Some("first"),
                    serde_json::json!({ "msgtype": "m.text", "body": "first" }),
                ),
                event_with_id(
                    "$two:example.com",
                    "m.room.message",
                    Some("second"),
                    serde_json::json!({ "msgtype": "m.text", "body": "second" }),
                ),
            ],
        );
        app.messages.selection = Some("$one:example.com".to_owned());
        app.start_edit_selected_message();

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
            .await;

        assert_eq!(app.mode, Mode::MessageList);
        assert_eq!(app.input.buffer, "");
        assert_eq!(app.input.cursor, 0);
        assert_eq!(app.selected_message_id(), Some("$two:example.com"));
    }

    #[tokio::test]
    async fn focus_cycle_abandons_edit_mode_to_compose() {
        let mut app = app_with_rooms(Vec::new());
        app.mode = Mode::Editing {
            event_id: "$old:example.com".to_owned(),
        };
        app.input.buffer = "old body".to_owned();
        app.input.cursor = app.input.buffer.len();

        app.handle_key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE))
            .await;

        assert_eq!(app.mode, Mode::Compose);
        assert_eq!(app.input.buffer, "");
        assert_eq!(app.input.cursor, 0);
    }

    #[tokio::test]
    async fn action_shortcuts_do_not_steal_compose_text_input() {
        for text in ["testing", "editing", "dog", "replying", "Reacting"] {
            let mut app = app_with_rooms(Vec::new());
            for ch in text.chars() {
                app.handle_key(KeyEvent::from(KeyCode::Char(ch))).await;
            }

            assert_eq!(app.input.buffer, text);
            assert_eq!(app.status, "");
        }
    }

    #[tokio::test]
    async fn reaction_tab_completion_shows_and_cycles_matching_emoji() {
        let mut app = app_with_rooms(Vec::new());
        app.mode = Mode::Reacting {
            event_id: "$message:example.com".to_owned(),
        };
        app.input.buffer = "face".to_owned();
        app.input.cursor = app.input.buffer.len();

        app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
        let first_status = app.status.text(false);
        assert_eq!(app.input.react_tab, Some(0));
        assert!(first_status.contains("[1/"));
        assert!(first_status.contains("Tab/Shift-Tab to cycle, Enter to send"));

        app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
        let second_status = app.status.text(false);
        assert_eq!(app.input.react_tab, Some(1));
        assert!(second_status.contains("[2/"));
        assert_ne!(second_status, first_status);

        app.handle_key(KeyEvent::from(KeyCode::BackTab)).await;
        assert_eq!(app.input.react_tab, Some(0));
        assert_eq!(app.status.text(false), first_status);

        app.handle_key(KeyEvent::from(KeyCode::BackTab)).await;
        assert_eq!(app.input.react_tab, Some(emoji_matches("face").len() - 1));
        assert!(app.status.text(false).contains(&format!(
            "[{}/{}]",
            emoji_matches("face").len(),
            emoji_matches("face").len()
        )));
    }

    #[tokio::test]
    async fn reaction_submit_rejects_unknown_text_without_leaving_reacting_mode() {
        let mut app = app_with_rooms(Vec::new());
        app.mode = Mode::Reacting {
            event_id: "$message:example.com".to_owned(),
        };
        app.input.buffer = "not-a-known-emoji".to_owned();
        app.input.cursor = app.input.buffer.len();

        app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

        assert_eq!(
            app.mode,
            Mode::Reacting {
                event_id: "$message:example.com".to_owned()
            }
        );
        assert_eq!(app.input.buffer, "not-a-known-emoji");
        assert_eq!(
            app.status.text(false),
            "no emoji matches 'not-a-known-emoji'"
        );
    }

    #[test]
    fn reaction_input_accepts_only_known_or_selected_emoji() {
        let mut app = app_with_rooms(Vec::new());

        assert_eq!(app.take_reaction_key("🚀"), Some("🚀".to_owned()));
        assert_eq!(app.take_reaction_key("rocket"), Some("🚀".to_owned()));
        assert_eq!(app.take_reaction_key("not-a-known-emoji"), None);

        let matches = emoji_matches("face");
        assert!(matches.len() > 1);
        assert_eq!(app.take_reaction_key("face"), None);

        app.input.react_tab = Some(1);
        assert_eq!(
            app.take_reaction_key("face"),
            Some(matches[1].as_str().to_owned())
        );
    }

    #[test]
    fn react_command_argument_prepares_immediate_reaction_for_most_recent_message() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![event_with_id(
                "$message:example.com",
                "m.room.message",
                Some("message"),
                serde_json::json!({ "msgtype": "m.text", "body": "message" }),
            )],
        );

        assert_eq!(
            app.prepare_reaction("+1"),
            Ok(("$message:example.com".to_owned(), "👍".to_owned()))
        );
        assert_eq!(app.mode, Mode::Compose);
    }

    #[test]
    fn react_command_argument_rejects_unknown_emoji() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![event_with_id(
                "$message:example.com",
                "m.room.message",
                Some("message"),
                serde_json::json!({ "msgtype": "m.text", "body": "message" }),
            )],
        );

        assert_eq!(
            app.prepare_reaction("not-a-known-emoji"),
            Err("unknown or ambiguous emoji: not-a-known-emoji".to_owned())
        );
        assert_eq!(app.mode, Mode::Compose);
    }

    #[test]
    fn own_reactions_group_duplicate_keys_and_ignore_other_or_redacted_reactions() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        // The server-aggregated tally: the account user's own 👍 (two reaction
        // events, deduplicated to one count), a 🎉 from someone else (`me` false),
        // and no redacted 🚀 (the server drops it from the tally).
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![message_with_reactions(
                "$message:example.com",
                vec![
                    (
                        "👍",
                        tally(1, true, &["$one:example.com", "$two:example.com"]),
                    ),
                    ("🎉", tally(1, false, &[])),
                ],
            )],
        );

        assert_eq!(
            app.own_reactions_for("$message:example.com"),
            Ok(vec![OwnReaction {
                key: "👍".to_owned(),
                event_ids: vec!["$one:example.com".to_owned(), "$two:example.com".to_owned()],
            }])
        );
    }

    #[tokio::test]
    async fn unreact_with_multiple_reactions_enters_and_cycles_selection_mode() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.selection = Some("$message:example.com".to_owned());
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![message_with_reactions(
                "$message:example.com",
                vec![
                    ("🚀", tally(1, true, &["$rocket:example.com"])),
                    ("👍", tally(1, true, &["$thumb:example.com"])),
                ],
            )],
        );

        app.start_unreact_from_selected_message().await;

        let Mode::Unreacting {
            choices, selected, ..
        } = &app.mode
        else {
            panic!("expected unreact selection mode");
        };
        assert_eq!(choices.len(), 2);
        assert_eq!(*selected, 0);
        let first_status = app.status.text(false);

        app.handle_key(KeyEvent::from(KeyCode::Tab)).await;

        let Mode::Unreacting { selected, .. } = app.mode else {
            panic!("expected unreact selection mode");
        };
        assert_eq!(selected, 1);
        assert_ne!(app.status.text(false), first_status);

        app.handle_key(KeyEvent::from(KeyCode::BackTab)).await;

        let Mode::Unreacting { selected, .. } = app.mode else {
            panic!("expected unreact selection mode");
        };
        assert_eq!(selected, 0);
        assert_eq!(app.status.text(false), first_status);

        app.handle_key(KeyEvent::from(KeyCode::BackTab)).await;

        let Mode::Unreacting { selected, .. } = app.mode else {
            panic!("expected unreact selection mode");
        };
        assert_eq!(selected, 1);

        app.handle_key(KeyEvent::from(KeyCode::Esc)).await;
        assert_eq!(app.mode, Mode::Compose);
        assert_eq!(app.status.text(false), "unreact canceled");
    }

    #[tokio::test]
    async fn unreact_hotkey_targets_selected_message() {
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.selection = Some("$message:example.com".to_owned());
        app.mode = Mode::MessageList;
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![message_with_reactions(
                "$message:example.com",
                vec![
                    ("🚀", tally(1, true, &["$rocket:example.com"])),
                    ("👍", tally(1, true, &["$thumb:example.com"])),
                ],
            )],
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('U'), KeyModifiers::SHIFT))
            .await;

        assert!(matches!(app.mode, Mode::Unreacting { .. }));
    }

    #[test]
    fn reaction_badges_come_from_aggregated_tally_and_skip_redacted_messages() {
        // Badges are read from the message's server-aggregated tally; the server
        // has already dropped redacted reactions from it.
        let mut message =
            message_with_reactions("$message:example.com", vec![("👍", tally(1, false, &[]))]);

        let reactions = collect_reactions(std::slice::from_ref(&message));

        assert_eq!(
            reactions.get("$message:example.com"),
            Some(&vec![("👍".to_owned(), 1)])
        );

        // A redacted message shows no badges at all.
        message.redacted = true;
        assert!(collect_reactions(&[message]).is_empty());
    }

    #[test]
    fn local_react_makes_badge_and_unreact_available_before_reload() {
        // A successful react must update the target message's aggregated tally in
        // place: the collapsed timeline no longer carries the raw `m.reaction` row,
        // so without the optimistic patch the badge would not appear and the
        // reaction could not be withdrawn until the next full reload.
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![event_with_id(
                "$message:example.com",
                "m.room.message",
                Some("message"),
                serde_json::json!({ "msgtype": "m.text", "body": "message" }),
            )],
        );

        app.apply_local_reaction("$message:example.com", "👍", "$mine:example.com".to_owned());

        let events = &app.messages.events[&RoomKey::from(&room)];
        assert_eq!(
            collect_reactions(events).get("$message:example.com"),
            Some(&vec![("👍".to_owned(), 1)]),
            "badge appears immediately after react"
        );
        assert_eq!(
            app.own_reactions_for("$message:example.com"),
            Ok(vec![OwnReaction {
                key: "👍".to_owned(),
                event_ids: vec!["$mine:example.com".to_owned()],
            }]),
            "the reaction is withdrawable immediately after react"
        );
    }

    #[test]
    fn local_unreact_clears_badge_and_choice_before_reload() {
        // Withdrawing the only reaction must clear the badge and the unreact choice
        // in place; the redacted raw `m.reaction` row is absent from the collapsed
        // timeline, so the aggregate has to be patched directly.
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![message_with_reactions(
                "$message:example.com",
                vec![("👍", tally(1, true, &["$mine:example.com"]))],
            )],
        );

        app.remove_local_reaction(
            "$message:example.com",
            "👍",
            &["$mine:example.com".to_owned()],
        );

        let events = &app.messages.events[&RoomKey::from(&room)];
        assert!(
            collect_reactions(events).is_empty(),
            "badge disappears after the last reaction is withdrawn"
        );
        assert_eq!(
            app.own_reactions_for("$message:example.com"),
            Ok(Vec::new()),
            "no withdrawable reaction remains"
        );
        assert!(
            events[0].reactions.is_none(),
            "an emptied tally is cleared from the row"
        );
    }

    #[test]
    fn local_unreact_keeps_other_senders_count_and_drops_my_contribution() {
        // When others also reacted with the same key, withdrawing my reaction drops
        // only my distinct-sender contribution and clears `me`; the badge persists
        // with the remaining count and is no longer withdrawable by me.
        let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
        let mut app = app_with_rooms(vec![room.clone()]);
        app.rooms.selected = Some(0);
        app.messages.events.insert(
            RoomKey::from(&room),
            vec![message_with_reactions(
                "$message:example.com",
                vec![("👍", tally(2, true, &["$mine:example.com"]))],
            )],
        );

        app.remove_local_reaction(
            "$message:example.com",
            "👍",
            &["$mine:example.com".to_owned()],
        );

        let events = &app.messages.events[&RoomKey::from(&room)];
        assert_eq!(
            collect_reactions(events).get("$message:example.com"),
            Some(&vec![("👍".to_owned(), 1)]),
            "the other sender's reaction still shows"
        );
        assert_eq!(
            app.own_reactions_for("$message:example.com"),
            Ok(Vec::new()),
            "I can no longer withdraw a reaction I removed"
        );
    }

    #[test]
    fn room_completion_fills_unique_room_alias_match() {
        let mut app = app_with_rooms(vec![
            room("!one:example.com", Some("#one:example.com"), Some("One")),
            room("!test:example.com", Some("#test:example.com"), Some("Test")),
        ]);
        app.input.buffer = "/room te".to_owned();

        app.complete_room_input(false);

        assert_eq!(app.input.buffer, "/room #test:example.com");
    }

    #[test]
    fn room_completion_ignores_rooms_hidden_by_account_filter() {
        let visible_account = Uuid::from_u128(1);
        let hidden_account = Uuid::from_u128(2);
        let mut visible = room("!visible:example.com", None, Some("General"));
        visible.account_id = visible_account;
        let mut hidden = room("!hidden:example.com", None, Some("General"));
        hidden.account_id = hidden_account;
        let mut app = app_with_rooms(vec![visible, hidden]);
        app.set_accounts(vec![
            account_with_id(
                visible_account,
                "@visible:example.com",
                AccountState::Active,
            ),
            account_with_id(hidden_account, "@hidden:example.com", AccountState::Active),
        ]);
        app.accounts.selected = AccountSelection::Account(0);
        app.input.buffer = "/room Gen".to_owned();

        app.complete_room_input(false);

        assert_eq!(app.input.buffer, "/room General");
        assert!(app.input.room_command_completion.is_none());
    }

    #[test]
    fn tab_completion_keeps_parsed_command_aliases_discoverable() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/roo".to_owned();

        app.complete_input();

        assert_eq!(app.input.buffer, "/roo");
        assert!(app.status.text(false).contains("/room, /rooms"));

        app.input.buffer = "/sw".to_owned();
        app.complete_input();

        assert_eq!(app.input.buffer, "/switch ");
    }

    #[tokio::test]
    async fn account_search_accepts_n_and_uppercase_n_as_query_text() {
        let mut app = app_with_rooms(Vec::new());
        app.mode = Mode::Search(SearchKind::Accounts, "a".to_owned());

        app.handle_key(KeyEvent::from(KeyCode::Char('n'))).await;
        app.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT))
            .await;

        assert_eq!(
            app.mode,
            Mode::Search(SearchKind::Accounts, "anN".to_owned())
        );
    }

    #[tokio::test]
    async fn submitting_account_search_selects_first_match() {
        let mut app = app_with_rooms(Vec::new());
        app.set_accounts(vec![
            account_with_id(
                Uuid::from_u128(1),
                "@alice:example.com",
                AccountState::Active,
            ),
            account_with_id(Uuid::from_u128(2), "@bob:example.com", AccountState::Active),
        ]);
        app.mode = Mode::Search(SearchKind::Accounts, "bob".to_owned());

        app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

        assert_eq!(app.mode, Mode::AccountList);
        assert_eq!(app.accounts.selected, AccountSelection::Account(1));
        assert_eq!(app.last_search.as_deref(), Some("bob"));
    }

    #[tokio::test]
    async fn submitting_account_search_reports_no_match() {
        let mut app = app_with_rooms(Vec::new());
        app.set_accounts(vec![account_with_id(
            Uuid::from_u128(1),
            "@alice:example.com",
            AccountState::Active,
        )]);
        app.mode = Mode::Search(SearchKind::Accounts, "missing".to_owned());

        app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

        assert_eq!(app.accounts.selected, AccountSelection::All);
        assert_eq!(app.status, "no account matches: missing");
    }

    #[test]
    fn account_numbers_match_panel_labels() {
        let mut app = app_with_rooms(Vec::new());
        app.set_accounts(vec![
            account_with_id(
                Uuid::from_u128(1),
                "@alice:example.com",
                AccountState::Active,
            ),
            account_with_id(Uuid::from_u128(2), "@bob:example.com", AccountState::Active),
        ]);

        assert!(app.switch_account("0"));
        assert_eq!(app.accounts.selected, AccountSelection::All);
        assert!(app.switch_account("2"));
        assert_eq!(app.accounts.selected, AccountSelection::Account(1));
        assert_eq!(AccountSelection::All.display_number(), 0);
        assert_eq!(AccountSelection::Account(1).display_number(), 2);

        app.accounts.selected = AccountSelection::All;
        assert!(app.commit_account_search("2".to_owned()));
        assert_eq!(app.accounts.selected, AccountSelection::Account(1));
    }

    #[test]
    fn logout_completion_cycles_only_matching_active_accounts() {
        let mut app = app_with_rooms(Vec::new());
        app.accounts.accounts = vec![
            account("@alice:example.com", AccountState::Active),
            account("@alice:work.example", AccountState::Active),
            account("@bob:example.com", AccountState::Active),
            account("@alice:old.example", AccountState::Deactivated),
        ];
        app.input.buffer = "/logout alice".to_owned();

        app.complete_input();
        assert_eq!(app.input.buffer, "/logout @alice:example.com");
        assert!(app.status.text(false).contains("[1/2]"));

        app.complete_input();
        assert_eq!(app.input.buffer, "/logout @alice:work.example");
        assert!(app.status.text(false).contains("[2/2]"));

        app.complete_input_reverse();
        assert_eq!(app.input.buffer, "/logout @alice:example.com");
    }

    #[test]
    fn logout_completion_without_target_cycles_all_active_accounts() {
        let mut app = app_with_rooms(Vec::new());
        app.accounts.accounts = vec![
            account("@alice:example.com", AccountState::Active),
            account("@bob:example.com", AccountState::Active),
            account("@old:example.com", AccountState::Deactivated),
        ];
        app.input.buffer = "/logout".to_owned();

        app.complete_input();

        assert_eq!(app.input.buffer, "/logout @alice:example.com");
        assert!(app.status.text(false).contains("[1/2]"));
    }

    #[test]
    fn logout_completion_normalizes_server_qualified_username_forms() {
        let mut app = app_with_rooms(Vec::new());
        app.accounts.accounts = vec![account("@alice:example.com", AccountState::Active)];

        app.input.buffer = "/logout alice:example.com".to_owned();
        app.complete_input();
        assert_eq!(app.input.buffer, "/logout @alice:example.com");

        app.input.logout_command_completion = None;
        app.input.buffer = "/logout alice@example.com".to_owned();
        app.complete_input();
        assert_eq!(app.input.buffer, "/logout @alice:example.com");
    }

    #[test]
    fn logout_completion_selects_duplicate_user_ids_by_account_id() {
        let first_id = Uuid::from_u128(1);
        let second_id = Uuid::from_u128(2);
        let mut app = app_with_rooms(Vec::new());
        app.accounts.accounts = vec![
            account_with_id(first_id, "@alice:example.com", AccountState::Active),
            account_with_id(second_id, "@alice:example.com", AccountState::Active),
        ];
        app.input.buffer = "/logout alice".to_owned();

        app.complete_input();
        assert_eq!(app.input.buffer, format!("/logout {first_id}"));

        app.complete_input();
        assert_eq!(app.input.buffer, format!("/logout {second_id}"));
        assert!(matches!(
            app.resolve_logout_target(Some(&second_id.to_string())),
            super::lifecycle::LogoutResolution::Match(AccountDto { account_id, .. })
                if account_id == second_id
        ));
    }

    #[test]
    fn delete_completion_selects_duplicate_user_ids_by_account_id() {
        let first_id = Uuid::from_u128(1);
        let second_id = Uuid::from_u128(2);
        let mut app = app_with_rooms(Vec::new());
        app.accounts.accounts = vec![
            account_with_id(first_id, "@alice:example.com", AccountState::Active),
            account_with_id(second_id, "@alice:example.com", AccountState::Deactivated),
        ];
        app.input.buffer = "/delete alice".to_owned();

        app.complete_input();
        assert_eq!(app.input.buffer, format!("/delete {first_id}"));

        app.complete_input();
        assert_eq!(app.input.buffer, format!("/delete {second_id}"));
        assert!(matches!(
            app.resolve_delete_target(Some(&second_id.to_string())),
            super::lifecycle::DeleteResolution::Match(AccountDto { account_id, .. })
                if account_id == second_id
        ));
    }

    #[test]
    fn verify_completion_matches_room_users_and_excludes_self() {
        let r = room("!dm:example.com", None, Some("DM"));
        let mut app = app_with_rooms(vec![r.clone()]);
        app.rooms.selected = Some(0);
        let mut names = HashMap::new();
        // The own user (@alice, from room.account_user_id) must be excluded.
        names.insert("@alice:example.com".to_owned(), "Alice".to_owned());
        names.insert("@bob:example.com".to_owned(), "Bob".to_owned());
        names.insert("@carol:example.com".to_owned(), "Carol".to_owned());
        app.rooms.display_names.insert(RoomKey::from(&r), names);

        // A localpart prefix resolves to the single matching user.
        app.input.buffer = "/verify @bo".to_owned();
        app.complete_input();
        assert_eq!(app.input.buffer, "/verify @bob:example.com");

        // An empty target cycles through every room user except our own.
        app.input.buffer = "/verify ".to_owned();
        app.input.verify_command_completion = None;
        app.complete_input();
        assert_eq!(app.input.buffer, "/verify @bob:example.com");
        app.complete_input();
        assert_eq!(app.input.buffer, "/verify @carol:example.com");
    }

    #[test]
    fn logout_prompts_for_confirmation_when_enabled() {
        let mut app = app_with_rooms(Vec::new());
        app.display.confirm_logout = true;

        app.request_logout(account("@alice:example.com", AccountState::Active));

        assert!(matches!(app.mode, Mode::ConfirmLogout { .. }));
        assert!(app
            .status
            .text(false)
            .contains("Log out @alice:example.com"));
    }

    #[test]
    fn logout_skips_confirmation_when_disabled() {
        let mut app = app_with_rooms(Vec::new());
        app.display.confirm_logout = false;

        app.request_logout(account("@alice:example.com", AccountState::Active));

        // Without a lifecycle sender the spawned logout is a no-op in tests, but
        // we should never have entered the confirmation prompt.
        assert!(!matches!(app.mode, Mode::ConfirmLogout { .. }));
        assert_eq!(app.mode, Mode::Compose);
    }

    #[tokio::test]
    async fn logout_confirmation_cancels_on_no() {
        let mut app = app_with_rooms(Vec::new());
        app.mode = Mode::ConfirmLogout {
            account: account("@alice:example.com", AccountState::Active),
        };

        app.handle_key(KeyEvent::from(KeyCode::Char('n'))).await;

        assert_eq!(app.mode, Mode::Compose);
        assert_eq!(app.status.text(false), "");
    }

    #[tokio::test]
    async fn logout_confirmation_ignores_unrelated_keys() {
        let mut app = app_with_rooms(Vec::new());
        app.mode = Mode::ConfirmLogout {
            account: account("@alice:example.com", AccountState::Active),
        };

        app.handle_key(KeyEvent::from(KeyCode::Char('x'))).await;

        assert!(matches!(app.mode, Mode::ConfirmLogout { .. }));
    }

    #[tokio::test]
    async fn delete_confirmation_non_yes_enter_clears_buffer() {
        let mut app = app_with_rooms(Vec::new());
        app.mode = Mode::ConfirmDelete {
            account: account("@alice:example.com", AccountState::Active),
        };
        app.input.buffer = "no".to_owned();
        app.input.cursor = app.input.buffer.len();

        app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

        assert_eq!(app.mode, Mode::Compose);
        assert!(app.input.buffer.is_empty());
    }

    #[tokio::test]
    async fn delete_confirmation_escape_clears_buffer() {
        let mut app = app_with_rooms(Vec::new());
        app.mode = Mode::ConfirmDelete {
            account: account("@alice:example.com", AccountState::Active),
        };
        app.input.buffer = "YES".to_owned();
        app.input.cursor = app.input.buffer.len();

        app.handle_key(KeyEvent::from(KeyCode::Esc)).await;

        assert_eq!(app.mode, Mode::Compose);
        assert!(app.input.buffer.is_empty());
    }

    #[tokio::test]
    async fn in_flight_lifecycle_rejects_new_login_and_logout() {
        let mut app = app_with_rooms(Vec::new());
        app.lifecycle_busy = true;

        app.handle_command(Command::Login {
            username: None,
            password: None,
            homeserver: None,
        })
        .await;
        assert_eq!(app.mode, Mode::Compose);
        assert!(app.status.text(false).contains("already in progress"));

        app.status = Status::Info(String::new());
        app.handle_command(Command::Logout(None)).await;
        assert!(app.status.text(false).contains("already in progress"));
    }

    #[tokio::test]
    async fn login_without_arguments_prompts_for_username_and_escape_clears_it() {
        let mut app = app_with_rooms(Vec::new());

        app.handle_command(Command::Login {
            username: None,
            password: None,
            homeserver: None,
        })
        .await;
        assert_eq!(app.mode, Mode::LoginUsername);

        app.input.buffer = "@alice:example.com".to_owned();
        app.input.cursor = app.input.buffer.len();
        app.handle_key(KeyEvent::from(KeyCode::Esc)).await;

        assert_eq!(app.mode, Mode::Compose);
        assert!(app.input.buffer.is_empty());
        assert_eq!(app.status.text(false), "");
    }

    #[tokio::test]
    async fn empty_recovery_key_skips_post_login_recovery() {
        let mut app = app_with_rooms(Vec::new());
        let account = account("@alice:example.com", AccountState::Active);
        app.mode = Mode::RecoveryKey {
            account,
            origin: RecoveryOrigin::PostLogin,
        };

        app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

        assert_eq!(app.mode, Mode::Compose);
        assert_eq!(
            app.status.text(false),
            "recovery skipped for @alice:example.com"
        );
        assert!(app.input.buffer.is_empty());
    }

    #[tokio::test]
    async fn escape_cancels_standalone_recovery_and_clears_secret() {
        let mut app = app_with_rooms(Vec::new());
        let account = account("@alice:example.com", AccountState::Active);
        app.mode = Mode::RecoveryKey {
            account,
            origin: RecoveryOrigin::Command,
        };
        app.input.buffer = "secret recovery key".to_owned();
        app.input.cursor = app.input.buffer.len();

        app.handle_key(KeyEvent::from(KeyCode::Esc)).await;

        assert_eq!(app.mode, Mode::Compose);
        assert_eq!(
            app.status.text(false),
            "recovery cancelled for @alice:example.com"
        );
        assert!(app.input.buffer.is_empty());
    }

    #[tokio::test]
    async fn account_navigation_clears_recovery_key_input() {
        let mut app = app_with_rooms(Vec::new());
        app.set_accounts(vec![
            account_with_id(
                Uuid::from_u128(1),
                "@alice:example.com",
                AccountState::Active,
            ),
            account_with_id(Uuid::from_u128(2), "@bob:example.com", AccountState::Active),
        ]);
        app.mode = Mode::RecoveryKey {
            account: account("@alice:example.com", AccountState::Active),
            origin: RecoveryOrigin::Command,
        };
        app.input.buffer = "secret recovery key".to_owned();
        app.input.cursor = app.input.buffer.len();

        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::ALT))
            .await;

        assert_eq!(app.mode, Mode::Compose);
        assert!(app.input.buffer.is_empty());
    }

    #[test]
    fn recover_resolution_uses_active_accounts_and_uuid_for_duplicates() {
        let first_id = Uuid::from_u128(1);
        let second_id = Uuid::from_u128(2);
        let mut app = app_with_rooms(Vec::new());
        app.accounts.accounts = vec![
            account_with_id(first_id, "@alice:example.com", AccountState::Active),
            account_with_id(second_id, "@alice:example.com", AccountState::Active),
            account_with_id(
                Uuid::from_u128(3),
                "@bob:example.com",
                AccountState::Deactivated,
            ),
        ];

        assert!(matches!(
            app.resolve_recover_target(Some(&second_id.to_string())),
            super::lifecycle::RecoverResolution::Match(AccountDto { account_id, .. })
                if account_id == second_id
        ));
        assert!(matches!(
            app.resolve_recover_target(Some("bob")),
            super::lifecycle::RecoverResolution::Missing
        ));

        app.input.buffer = "/recover alice".to_owned();
        app.complete_input();
        assert_eq!(app.input.buffer, format!("/recover {first_id}"));
        app.complete_input();
        assert_eq!(app.input.buffer, format!("/recover {second_id}"));
    }

    #[tokio::test]
    async fn invalid_login_username_stays_editable() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "alice".to_owned();
        app.input.cursor = app.input.buffer.len();
        app.mode = Mode::LoginUsername;

        app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

        assert_eq!(app.mode, Mode::LoginUsername);
        assert_eq!(app.input.buffer, "alice");
        assert!(app.status.text(false).contains("name@domain"));
    }

    #[tokio::test]
    async fn login_username_prompt_canonicalizes_common_email_style() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "alice@example.com".to_owned();
        app.input.cursor = app.input.buffer.len();
        app.mode = Mode::LoginUsername;

        app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

        assert_eq!(
            app.mode,
            Mode::LoginPassword {
                username: "@alice:example.com".to_owned(),
                homeserver: None,
            }
        );
        assert!(app.input.buffer.is_empty());
    }

    #[tokio::test]
    async fn login_username_prompt_captures_optional_homeserver() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "@alice:example.com hs.example.org".to_owned();
        app.input.cursor = app.input.buffer.len();
        app.mode = Mode::LoginUsername;

        app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

        assert_eq!(
            app.mode,
            Mode::LoginPassword {
                username: "@alice:example.com".to_owned(),
                homeserver: Some("https://hs.example.org".to_owned()),
            }
        );
    }

    #[tokio::test]
    async fn login_username_prompt_rejects_extra_tokens() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "@alice:example.com hs.example.org junk".to_owned();
        app.input.cursor = app.input.buffer.len();
        app.mode = Mode::LoginUsername;

        app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

        // Stays on the username step with the input intact for correction.
        assert_eq!(app.mode, Mode::LoginUsername);
        assert_eq!(app.input.buffer, "@alice:example.com hs.example.org junk");
        assert!(app.status.text(false).contains("at most"));
    }

    #[test]
    fn tab_completion_fills_argument_slash_command_with_space() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/acco".to_owned();

        app.complete_input();

        assert_eq!(app.input.buffer, "/account ");
    }

    #[test]
    fn tab_completion_fills_help_command() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/he".to_owned();

        app.complete_input();

        assert_eq!(app.input.buffer, "/help");
    }

    #[test]
    fn tab_completion_fills_shortcuts_command() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/sh".to_owned();

        app.complete_input();

        assert_eq!(app.input.buffer, "/shortcuts");
    }

    #[test]
    fn tab_completion_fills_react_command_with_argument_space() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/rea".to_owned();

        app.complete_input();

        assert_eq!(app.input.buffer, "/react ");
    }

    #[test]
    fn tab_completion_cycles_emoji_matches_after_react_command() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/react face".to_owned();
        app.input.cursor = app.input.buffer.len();

        app.complete_input();
        let first = app.input.buffer.clone();
        assert!(first.starts_with("/react "));
        assert!(app.status.text(false).contains("[1/"));

        app.complete_input();
        let second = app.input.buffer.clone();
        assert!(app.status.text(false).contains("[2/"));
        assert_ne!(second, first);
    }

    #[tokio::test]
    async fn shift_tab_cycles_react_command_emoji_matches_backward() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/react face".to_owned();
        app.input.cursor = app.input.buffer.len();

        app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
        let first = app.input.buffer.clone();
        app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
        assert_ne!(app.input.buffer, first);

        app.handle_key(KeyEvent::from(KeyCode::BackTab)).await;
        assert_eq!(app.input.buffer, first);
        assert!(app.status.text(false).contains("[1/"));

        app.handle_key(KeyEvent::from(KeyCode::BackTab)).await;
        let match_count = emoji_matches("face").len();
        assert!(app
            .status
            .text(false)
            .contains(&format!("[{match_count}/{match_count}]")));
    }

    #[tokio::test]
    async fn compose_tab_completes_react_emoji_and_edit_resets_cycle() {
        let mut app = app_with_rooms(Vec::new());
        for ch in "/react face".chars() {
            app.handle_key(KeyEvent::from(KeyCode::Char(ch))).await;
        }

        app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
        assert!(app.input.react_command_completion.is_some());
        assert!(app.input.buffer.starts_with("/react "));

        app.handle_key(KeyEvent::from(KeyCode::Char('x'))).await;
        assert!(app.input.react_command_completion.is_none());
    }

    #[test]
    fn react_command_emoji_completion_reports_no_matches() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/react not-a-known-emoji".to_owned();

        app.complete_input();

        assert_eq!(
            app.status.text(false),
            "no emoji matches 'not-a-known-emoji'"
        );
        assert_eq!(app.input.buffer, "/react not-a-known-emoji");
    }

    #[test]
    fn tab_completion_fills_filter_argument() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/filter un".to_owned();

        app.complete_input();

        assert_eq!(app.input.buffer, "/filter unread");
        assert_eq!(
            app.status.text(false),
            "[1/1] unread - Tab/Shift-Tab to cycle, Enter to filter"
        );
    }

    #[test]
    fn tab_completion_cycles_filter_argument_aliases() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/filter g".to_owned();

        app.complete_input();
        assert_eq!(app.input.buffer, "/filter groups");

        app.complete_input();
        assert_eq!(app.input.buffer, "/filter group");

        app.complete_input_reverse();
        assert_eq!(app.input.buffer, "/filter groups");
    }

    #[test]
    fn tab_completion_cycles_filter_arguments_without_target() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/filter ".to_owned();

        app.complete_input();
        assert_eq!(app.input.buffer, "/filter all");
        assert!(app.status.text(false).contains("[1/11] all"));
    }

    #[tokio::test]
    async fn filter_completion_edit_resets_cycle() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/filter g".to_owned();
        app.input.cursor = app.input.buffer.len();

        app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
        assert!(app.input.filter_command_completion.is_some());

        app.handle_key(KeyEvent::from(KeyCode::Char('x'))).await;
        assert!(app.input.filter_command_completion.is_none());
    }

    #[test]
    fn tab_completion_fills_unreact_command() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/unreac".to_owned();

        app.complete_input();

        assert_eq!(app.input.buffer, "/unreact");
    }

    #[test]
    fn tab_completion_reports_ambiguous_slash_command() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/".to_owned();

        app.complete_input();

        assert_eq!(app.input.buffer, "/");
        assert!(app.status.text(false).contains("/room"));
        assert!(app.status.text(false).contains("/status"));
        assert!(app.status.text(false).contains("/event"));
        assert!(app.status.text(false).contains("/whoami"));
        assert!(app.status.text(false).contains("/whereami"));
        assert!(app.status.text(false).contains("/react"));
        assert!(app.status.text(false).contains("/unreact"));
        assert!(app.status.text(false).contains("/reply"));
        assert!(app.status.text(false).contains("/thread"));
        assert!(app.status.text(false).contains("/help"));
        assert!(app.status.text(false).contains("/shortcuts"));
        assert!(app.status.text(false).contains("/refresh"));
        assert!(app.status.text(false).contains("/quit"));
        assert!(app.status.text(false).contains("/join"));
        assert!(app.status.text(false).contains("/leave"));
        assert!(app.status.text(false).contains("/part"));
    }

    #[test]
    fn tab_completion_fills_refresh_command() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/ref".to_owned();

        app.complete_input();

        assert_eq!(app.input.buffer, "/refresh");
    }

    #[test]
    fn tab_completion_fills_known_api_unsupported_command() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/jo".to_owned();

        app.complete_input();

        assert_eq!(app.input.buffer, "/join ");
    }

    #[test]
    fn tab_completion_fills_whoami_command() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/who".to_owned();

        app.complete_input();

        assert_eq!(app.input.buffer, "/whoami");
    }

    #[test]
    fn tab_completion_fills_whereami_command() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/where".to_owned();

        app.complete_input();

        assert_eq!(app.input.buffer, "/whereami");
    }

    #[tokio::test]
    async fn popup_keys_scroll_and_close_popup() {
        let mut app = app_with_rooms(Vec::new());
        app.mode = Mode::Popup(PopupKind::CommandResponse);
        app.pending_command_response = Some("full command response".to_owned());

        app.handle_key(KeyEvent::from(KeyCode::Down)).await;
        assert_eq!(app.popup_scroll, 1);

        app.handle_key(KeyEvent::from(KeyCode::PageDown)).await;
        assert_eq!(app.popup_scroll, 9);

        app.handle_key(KeyEvent::from(KeyCode::PageUp)).await;
        assert_eq!(app.popup_scroll, 1);

        app.handle_key(KeyEvent::from(KeyCode::Esc)).await;
        assert_eq!(app.popup_scroll, 0);
        assert_eq!(app.mode, Mode::Compose);
        assert!(app.pending_command_response.is_none());
    }

    #[tokio::test]
    async fn dismissing_search_help_popup_clears_entry_status() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "stale".to_owned();
        app.input.cursor = app.input.buffer.len();
        app.status = Status::Info(crate::search::SEARCH_HELP_TEXT.to_owned());
        app.pending_command_response = Some(crate::search::SEARCH_HELP_TEXT.to_owned());
        app.mode = Mode::Popup(PopupKind::CommandResponse);

        app.handle_key(KeyEvent::from(KeyCode::Esc)).await;

        assert_eq!(app.mode, Mode::Compose);
        assert_eq!(app.status.text(false), "");
        assert!(app.pending_command_response.is_none());
        assert!(app.input.buffer.is_empty());
        assert_eq!(app.input.cursor, 0);
    }

    #[tokio::test]
    async fn help_popup_selects_command_into_input() {
        let mut app = app_with_rooms(Vec::new());
        app.handle_command(Command::Help).await;

        // Down twice to reach "//<text>" (the new "Alt+Enter" newline entry sits
        // between it and "plain text").
        app.handle_key(KeyEvent::from(KeyCode::Down)).await;
        app.handle_key(KeyEvent::from(KeyCode::Down)).await;
        app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

        assert_eq!(app.mode, Mode::Compose);
        assert_eq!(app.input.buffer, "//");
        assert_eq!(app.input.cursor, "//".len());
        assert_eq!(app.status.text(false), "selected command: //<text>");
    }

    #[tokio::test]
    async fn help_popup_selection_wraps_and_esc_resets_it() {
        let mut app = app_with_rooms(Vec::new());
        app.handle_command(Command::Help).await;

        app.handle_key(KeyEvent::from(KeyCode::Up)).await;

        assert_eq!(app.help_selection, HELP_COMMANDS.len() - 1);

        app.handle_key(KeyEvent::from(KeyCode::Esc)).await;

        assert_eq!(app.mode, Mode::Compose);
        assert_eq!(app.popup_scroll, 0);
        assert_eq!(app.help_selection, 0);
    }

    #[test]
    fn shortcuts_popup_lists_all_configurable_shortcuts() {
        let config = TuiConfig::test_default();
        let text = popup_shortcuts_lines(&config.shortcuts)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("F6"));
        assert!(text.contains("Ctrl-N"));
        assert!(text.contains("Ctrl-P"));
        assert!(text.contains("Ctrl-J"));
        assert!(text.contains("Ctrl-K"));
        assert!(text.contains("PageUp"));
        assert!(text.contains("PageDown"));
        assert!(text.contains("select previous / next message"));
    }

    #[test]
    pub(crate) fn new_app_starts_with_one_time_input_help() {
        let app = App::new(
            AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            Picker::halfblocks(),
        );

        assert!(app.show_input_help);
        assert!(app.input.buffer.is_empty());
    }

    #[tokio::test]
    async fn first_input_action_dismisses_input_help() {
        let mut app = App::new(
            AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            Picker::halfblocks(),
        );

        app.handle_key(KeyEvent::from(KeyCode::Char('/'))).await;

        assert!(!app.show_input_help);
        assert_eq!(app.input.buffer, "/");
    }

    #[tokio::test]
    async fn room_switch_shortcut_dismisses_input_help_when_no_rooms_exist() {
        let mut app = App::new(
            AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            Picker::halfblocks(),
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
            .await;

        assert!(!app.show_input_help);
        assert_eq!(app.status, "no rooms to switch");
    }

    #[tokio::test]
    async fn room_switch_shortcut_abandons_edit_mode() {
        let mut app = app_with_rooms(Vec::new());
        app.mode = Mode::Editing {
            event_id: "$old:example.com".to_owned(),
        };
        app.input.buffer = "old body".to_owned();
        app.input.cursor = app.input.buffer.len();

        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
            .await;

        assert_eq!(app.mode, Mode::Compose);
        assert_eq!(app.input.buffer, "");
        assert_eq!(app.input.cursor, 0);
        assert_eq!(app.status, "no rooms to switch");
    }

    #[tokio::test]
    async fn search_results_edit_key_reopens_existing_query_form() {
        let mut app = app_with_rooms(Vec::new());
        let edit_form = crate::search::SearchFormState {
            scope: crate::search::SearchScope::SpecificAccount,
            query: "backup key".to_owned(),
            account: "@alice:example.org".to_owned(),
            sender: "@bob:example.org".to_owned(),
            ..Default::default()
        };
        app.search_results = Some(crate::search::SearchResultsState {
            request: crate::search::SearchRequest {
                q: "backup key".to_owned(),
                account_id: None,
                room_id: None,
                sender: Some("@bob:example.org".to_owned()),
                from: None,
                to: None,
                limit: crate::search::DEFAULT_SEARCH_LIMIT,
                cursor: None,
            },
            edit_form: edit_form.clone(),
            results: Vec::new(),
            total: 0,
            next_cursor: None,
            selected: 0,
            loading: false,
            sort_order: crate::search::SearchSortOrder::NewestFirst,
            grouping: crate::search::SearchGrouping::None,
            context_cache: Default::default(),
        });
        app.mode = Mode::SearchResults;

        app.handle_key(KeyEvent::from(KeyCode::Char('e'))).await;

        assert_eq!(app.mode, Mode::SearchForm);
        assert_eq!(app.search_form, edit_form);
        assert_eq!(app.status, "edit search");

        app.handle_key(KeyEvent::from(KeyCode::Esc)).await;

        assert_eq!(app.mode, Mode::SearchResults);
        assert!(app.search_results.is_some());
    }

    #[tokio::test]
    async fn search_form_current_account_requires_concrete_account() {
        let mut app = app_with_rooms(Vec::new());
        app.accounts.accounts = vec![
            account_with_id(
                Uuid::from_u128(1),
                "@alice:example.com",
                AccountState::Active,
            ),
            account_with_id(Uuid::from_u128(2), "@bob:example.com", AccountState::Active),
        ];
        app.accounts.selected = AccountSelection::All;
        app.search_form.scope = crate::search::SearchScope::CurrentAccount;
        app.search_form.query = "needle".to_owned();

        app.submit_search_form().await;

        assert_eq!(app.mode, Mode::SearchForm);
        assert_eq!(
            app.search_form.error.as_deref(),
            Some("select an account or choose all accounts")
        );
        assert_eq!(app.status, "select an account or choose all accounts");
    }

    #[test]
    fn tab_completion_reports_missing_slash_command() {
        let mut app = app_with_rooms(Vec::new());
        app.input.buffer = "/zzz".to_owned();

        app.complete_input();

        assert_eq!(app.input.buffer, "/zzz");
        assert_eq!(app.status, "no command matches: /zzz");
    }

    #[test]
    fn room_completion_adds_missing_hash_for_qualified_alias() {
        let mut app = app_with_rooms(vec![room(
            "!test:example.com",
            Some("#test:example.com"),
            Some("Test"),
        )]);
        app.input.buffer = "/room test:ex".to_owned();

        app.complete_room_input(false);

        assert_eq!(app.input.buffer, "/room #test:example.com");
    }

    #[test]
    fn room_completion_reports_ambiguous_room_matches() {
        let mut app = app_with_rooms(vec![
            room("!one:example.com", Some("#test:example.com"), Some("Test")),
            room(
                "!two:example.com",
                Some("#testing:example.com"),
                Some("Testing"),
            ),
        ]);
        app.input.buffer = "/room test".to_owned();

        app.complete_room_input(false);

        assert_eq!(app.input.buffer, "/room test");
        assert!(app.status.text(false).contains("#test:example.com"));
        assert!(app.status.text(false).contains("#testing:example.com"));
    }

    #[test]
    fn room_completion_extends_to_common_prefix_and_shows_suffixes() {
        let mut app = app_with_rooms(vec![
            room("!one:example.com", None, Some("axontest")),
            room("!two:example.com", None, Some("axondev")),
        ]);
        app.input.buffer = "/room ax".to_owned();

        app.complete_room_input(false);

        assert_eq!(app.input.buffer, "/room axon");
        assert!(app.status.text(false).contains("test"));
        assert!(app.status.text(false).contains("dev"));
    }

    #[tokio::test]
    async fn enter_does_not_submit_partial_switch_completion() {
        let mut app = app_with_rooms(vec![
            room("!one:example.com", None, Some("axontest")),
            room("!two:example.com", None, Some("axondev")),
        ]);
        app.input.buffer = "/room ax".to_owned();
        app.input.cursor = app.input.buffer.len();

        app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
        assert_eq!(app.input.buffer, "/room axon");
        assert_eq!(
            app.input.partial_room_completions,
            Some(vec!["test".to_owned(), "dev".to_owned()])
        );

        app.handle_key(KeyEvent::from(KeyCode::Enter)).await;

        assert_eq!(app.input.buffer, "/room axon");
        assert_eq!(app.rooms.selected, None);
        assert_eq!(
            app.status.text(false),
            "room completion is partial: test, dev - type more or press Tab"
        );

        app.handle_key(KeyEvent::from(KeyCode::Char('t'))).await;
        assert!(app.input.partial_room_completions.is_none());
    }

    #[test]
    fn room_completion_uses_matching_names_when_rooms_have_aliases() {
        let mut app = app_with_rooms(vec![
            room(
                "!one:example.com",
                Some("#test:example.com"),
                Some("axontest"),
            ),
            room(
                "!two:example.com",
                Some("#dev:example.com"),
                Some("axondev"),
            ),
        ]);
        app.input.buffer = "/room ax".to_owned();

        app.complete_room_input(false);

        assert_eq!(app.input.buffer, "/room axon");
        assert!(app.status.text(false).contains("test"));
        assert!(app.status.text(false).contains("dev"));
    }

    #[test]
    fn room_completion_still_completes_unique_match_fully() {
        let mut app = app_with_rooms(vec![
            room("!one:example.com", None, Some("axontest")),
            room("!two:example.com", None, Some("axondev")),
        ]);
        app.input.buffer = "/room axont".to_owned();

        app.complete_room_input(false);

        assert_eq!(app.input.buffer, "/room axontest");
    }

    #[test]
    fn room_completion_replaces_unique_name_match_with_canonical_alias() {
        let mut app = app_with_rooms(vec![
            room(
                "!one:example.com",
                Some("#test:example.com"),
                Some("axontest"),
            ),
            room(
                "!two:example.com",
                Some("#dev:example.com"),
                Some("axondev"),
            ),
        ]);
        app.input.buffer = "/room axont".to_owned();

        app.complete_room_input(false);

        assert_eq!(app.input.buffer, "/room #test:example.com");
    }

    #[test]
    fn room_completion_cycles_duplicate_named_rooms_with_disambiguator() {
        let mut app = app_with_rooms(vec![
            room("!one:example.com", None, Some("General")),
            room("!two:example.com", None, Some("General")),
        ]);
        app.input.buffer = "/room General".to_owned();

        app.complete_room_input(false);
        assert_eq!(app.input.buffer, "/room !one:example.com");
        assert!(app.status.text(false).contains("[1/2]"));
        assert!(app.status.text(false).contains("General"));
        assert!(app.status.text(false).contains("!one:example.com"));
        assert!(app.status.text(false).contains("Tab/Shift-Tab to cycle"));

        app.complete_room_input(false);
        assert_eq!(app.input.buffer, "/room !two:example.com");
        assert!(app.status.text(false).contains("[2/2]"));
        assert!(app.status.text(false).contains("!two:example.com"));

        app.complete_room_input(true);
        assert_eq!(app.input.buffer, "/room !one:example.com");
        assert!(app.status.text(false).contains("[1/2]"));
    }

    #[test]
    fn room_completion_deduplicates_same_room_across_accounts() {
        // Regression: the same Matrix room joined by two accounts appears twice
        // in the room list (one per account_id). If one account hasn't synced the
        // canonical_alias state event yet, the room shows up as both
        // "#scratch:example.com" and "scratch", producing a spurious third match.
        // visible_rooms_for_completion deduplicates by room_id, keeping the entry
        // with a canonical alias.
        let mut account_b_entry = room("!scratch:example.com", None, Some("scratch"));
        account_b_entry.account_id = Uuid::from_u128(2);

        let mut app = app_with_rooms(vec![
            room(
                "!scratch:example.com",
                Some("#scratch:example.com"),
                Some("scratch"),
            ),
            room(
                "!scratch2:example.com",
                Some("#scratch-2:example.com"),
                Some("scratch-2"),
            ),
            account_b_entry,
        ]);
        app.input.buffer = "/room scratch".to_owned();

        app.complete_room_input(false);

        // Should see exactly 2 candidates, not 3.
        let status = app.status.text(false);
        assert!(
            status.contains("#scratch:example.com"),
            "expected alias in status: {status}"
        );
        assert!(
            status.contains("#scratch-2:example.com"),
            "expected alias-2 in status: {status}"
        );
        assert!(
            !status.contains("completions: #scratch:example.com, #scratch-2:example.com, scratch"),
            "spurious bare 'scratch' entry in status: {status}"
        );
    }

    #[tokio::test]
    async fn room_completion_enter_selects_after_prefix_expansion_then_cycling() {
        // Regression: partial_room_completions set during prefix expansion must be
        // cleared when cycling begins, otherwise Enter is incorrectly blocked.
        let mut app = app_with_rooms(vec![
            room("!one:example.com", None, Some("General")),
            room("!two:example.com", None, Some("General")),
        ]);
        app.input.buffer = "/room G".to_owned();
        app.input.cursor = app.input.buffer.len();

        // First Tab: prefix-expands "G" → "General", sets partial_room_completions
        app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
        assert_eq!(app.input.buffer, "/room General");
        assert!(app.input.partial_room_completions.is_some());

        // Second Tab: enters cycling mode — partial_room_completions must be cleared
        app.handle_key(KeyEvent::from(KeyCode::Tab)).await;
        assert!(app.input.buffer.starts_with("/room !"));
        assert!(app.input.partial_room_completions.is_none());

        // Enter must not be blocked
        app.handle_key(KeyEvent::from(KeyCode::Enter)).await;
        assert!(app.rooms.selected.is_some());
    }

    #[test]
    fn room_completion_typing_after_cycling_resets_to_normal_completion() {
        let mut app = app_with_rooms(vec![
            room("!one:example.com", None, Some("General")),
            room("!two:example.com", None, Some("General")),
        ]);
        app.input.buffer = "/room General".to_owned();
        app.input.cursor = app.input.buffer.len();

        app.complete_room_input(false);
        assert!(app.input.room_command_completion.is_some());

        app.insert_char('x');
        assert!(app.input.room_command_completion.is_none());
    }

    #[test]
    fn room_resolution_accepts_unique_name_prefix() {
        let app = app_with_rooms(vec![
            room(
                "!one:example.com",
                Some("#test:example.com"),
                Some("axontest"),
            ),
            room(
                "!two:example.com",
                Some("#dev:example.com"),
                Some("axondev"),
            ),
        ]);

        assert_eq!(
            app.resolve_room_target("axont"),
            RoomTargetResolution::Match(0)
        );
        assert_eq!(
            app.resolve_room_target("axon"),
            RoomTargetResolution::Ambiguous(vec!["test".to_owned(), "dev".to_owned()])
        );
    }

    #[test]
    fn room_resolution_can_be_scoped_to_account() {
        let account_a = Uuid::from_u128(1);
        let account_b = Uuid::from_u128(2);
        let mut first = room("!one:example.com", None, Some("General"));
        first.account_id = account_a;
        let mut second = room("!two:example.com", None, Some("General"));
        second.account_id = account_b;
        let mut app = app_with_rooms(vec![first, second]);
        app.accounts.accounts = vec![
            account_with_id(account_a, "@alice:example.com", AccountState::Active),
            account_with_id(account_b, "@bob:example.com", AccountState::Active),
        ];
        app.accounts.selected = AccountSelection::Account(0);

        assert_eq!(
            app.resolve_room_target("General"),
            RoomTargetResolution::Match(0)
        );
        assert_eq!(
            app.resolve_room_target_in_account("General", None),
            RoomTargetResolution::Ambiguous(vec!["General".to_owned(), "General".to_owned()])
        );
        assert_eq!(
            app.resolve_room_target_in_account("General", Some(account_b)),
            RoomTargetResolution::Match(1)
        );
    }

    #[tokio::test]
    async fn switch_command_reports_ambiguous_name_suffixes() {
        let mut app = app_with_rooms(vec![
            room(
                "!one:example.com",
                Some("#test:example.com"),
                Some("axontest"),
            ),
            room(
                "!two:example.com",
                Some("#dev:example.com"),
                Some("axondev"),
            ),
        ]);

        app.handle_command(Command::Room("axon".to_owned())).await;

        assert_eq!(app.status.text(false), "room name is ambiguous: test, dev");
        assert_eq!(app.rooms.selected, None);
    }

    #[test]
    fn room_completion_only_runs_for_switch_command() {
        let mut app = app_with_rooms(vec![room(
            "!test:example.com",
            Some("#test:example.com"),
            Some("Test"),
        )]);
        app.input.buffer = "/event te".to_owned();

        app.complete_input();

        assert_eq!(app.input.buffer, "/event te");
    }

    #[test]
    pub(crate) fn find_room_adds_missing_hash_to_fully_qualified_alias() {
        let app = app_with_rooms(vec![room(
            "!abc:example.com",
            Some("#test:example.com"),
            Some("Test Room"),
        )]);

        assert_eq!(
            app.resolve_room_target("test:example.com"),
            RoomTargetResolution::Match(0)
        );
        assert_eq!(
            app.resolve_room_target("test:other.example"),
            RoomTargetResolution::Missing
        );
    }

    #[test]
    pub(crate) fn find_room_keeps_exact_alias_and_name_matches() {
        let app = app_with_rooms(vec![room(
            "!abc:example.com",
            Some("#test:example.com"),
            Some("Friendly Name"),
        )]);

        assert_eq!(
            app.resolve_room_target("#test:example.com"),
            RoomTargetResolution::Match(0)
        );
        assert_eq!(
            app.resolve_room_target("friendly name"),
            RoomTargetResolution::Match(0)
        );
    }

    #[test]
    pub(crate) fn find_room_does_not_local_match_fully_qualified_wrong_server() {
        let app = app_with_rooms(vec![room(
            "!abc:example.com",
            Some("#test:example.com"),
            Some("Test Room"),
        )]);

        assert_eq!(
            app.resolve_room_target("#test:other.example"),
            RoomTargetResolution::Missing
        );
    }
}
