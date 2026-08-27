use ratatui::layout::{Rect, Size};
use ratatui_image::picker::Picker;
use std::collections::{HashMap, HashSet, VecDeque};
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
mod bootstrap;
mod completion;
mod drafts;
mod ephemeral;
pub(crate) use drafts::{load_or_create_device_id, DraftOutcome};
mod layout_cache;
mod lifecycle;
mod read_markers;
pub(crate) use bootstrap::{BootstrapOutcome, BootstrapStage};
pub(crate) use lifecycle::LifecycleOutcome;
pub(crate) mod media;
use media::{evict_lru_where, touch_lru};
pub(crate) use media::{
    ImageState, MediaKey, MediaResult, ProtocolKey, ProtocolState, IMAGE_CACHE_LIMIT,
    MEDIA_WORKERS, PROTOCOL_CACHE_LIMIT,
};
mod panels;
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
    overlay_selection_on_page, selected_line_style, ImageThumbRows, RelationContext, ReplyPreview,
    ThreadBadge, IMAGE_THUMB_ROWS,
};
pub(crate) use room_actions::{PendingRoomAction, RoomActionOutcome};
pub(crate) use rooms::{account_localpart, apply_edits, dm_title_from_members};
#[cfg(test)]
use timeline::should_show_event;
use timeline::MEMBERS_WORKERS;

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
    /// `/backup enable` reuses the masked recovery-key prompt. Empty Enter
    /// kicks upload only; it does not skip.
    BackupEnable,
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
    /// Delivers completed startup stages to the main loop (#189).
    pub(crate) bootstrap_tx: Option<mpsc::UnboundedSender<BootstrapOutcome>>,
    /// How far startup has got; drives the rooms-panel loading label.
    pub(crate) bootstrap: BootstrapStage,
    /// How long each startup stage took, for the `display.debug` overlay: a
    /// user reporting "slow start" can read the breakdown off their own screen
    /// (#189). `None` until the stage completes.
    pub(crate) bootstrap_timings: Vec<(&'static str, std::time::Duration)>,
    /// When the current startup stage began.
    pub(crate) bootstrap_stage_started: Instant,
    /// Whether a `Connected` live frame has already been seen. Exactly the
    /// first one is redundant with startup's own device-state fetch; every
    /// later one is a genuine reconnect whose dropped frames must be re-read,
    /// whether or not startup has finished by then (#210).
    pub(crate) seen_first_connect: bool,
    /// A device-state read is in flight, so another must not be started: two
    /// overlapping reads landing out of order would let the older view delete a
    /// draft the newer one installed. See `App::request_device_state`.
    pub(crate) device_state_inflight: bool,
    /// A device-state read was asked for while one was in flight; run one more
    /// when it lands rather than one per request.
    pub(crate) device_state_again: bool,
    /// Draft keys written by a live `device_state` frame since the in-flight
    /// read was dispatched. Those writes are newer than the view that read will
    /// return, so `apply_draft_reads` must not mistake them for keys the server
    /// tombstoned.
    pub(crate) drafts_written_since_fetch: std::collections::HashSet<RoomKey>,
    /// Bumped on config reload. Stands in for the config-level inputs to the
    /// message layout (`app::layout_cache`).
    pub(crate) config_generation: u64,
    /// A room-list fetch is in flight, so another must not be started.
    pub(crate) rooms_fetch_inflight: bool,
    /// A room-list fetch was asked for while one was in flight; run one more
    /// when it lands rather than one per request.
    pub(crate) rooms_fetch_again: bool,
    /// Whether a room was selected when the in-flight fetch was requested, so
    /// the handler knows if the refresh revealed the session's first room.
    pub(crate) rooms_fetch_had_selection: bool,
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
    /// Rooms whose `/members` read produced no derivable list title (no other
    /// member to name them after). Without an explicit record the title sweep
    /// asks again every cooldown, forever, on every refresh (#189).
    ///
    /// Deliberately separate from `members_refresh_after`: that map also rate
    /// limits the live unknown-sender path, and suppressing *display name*
    /// refreshes for an hour because a room has no derivable title would be a
    /// different behaviour change entirely.
    pub(crate) rooms_without_derived_title: HashSet<RoomKey>,
    /// Bounds concurrent `/members` reads, as `media_workers` bounds image
    /// work. A room list with thousands of unnamed rooms used to fan out one
    /// unthrottled request per room (#189).
    members_workers: Arc<Semaphore>,
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
    /// Per-image body heights the cached `layout` was built from, kept so
    /// `draw` reads them instead of rederiving the same O(events) filter+map
    /// every frame.
    pub(crate) layout_image_thumb_rows: crate::app::render::ImageThumbRows,
    /// Digest of everything the cached `layout` was computed from (#54).
    /// `None` until the first layout runs.
    pub(crate) layout_key: Option<u64>,
    /// How many times `ensure_message_layout` has been asked for a layout, and
    /// how many of those actually re-ran it.
    ///
    /// Surfaced in the `display.debug` overlay because a cache that never hits
    /// is invisible: it renders identically and only costs more. Without these
    /// two numbers a broken digest and a working one look exactly the same on
    /// screen (#54).
    pub(crate) layout_checks: u64,
    pub(crate) layout_recomputes: u64,
    /// The layout `draw` renders and the nav math measures against. One per
    /// change rather than two per frame; see `app::layout_cache`.
    pub(crate) layout: Option<crate::app::render::MessageLayout>,
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
            layout_image_thumb_rows: crate::app::render::ImageThumbRows::new(),
            layout_key: None,
            layout_checks: 0,
            layout_recomputes: 0,
            layout: None,
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
    pub(crate) backup_command_completion: Option<(String, usize)>,
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
            bootstrap_tx: None,
            bootstrap: BootstrapStage::Accounts,
            bootstrap_timings: Vec::new(),
            bootstrap_stage_started: Instant::now(),
            seen_first_connect: false,
            device_state_inflight: false,
            device_state_again: false,
            drafts_written_since_fetch: std::collections::HashSet::new(),
            config_generation: 0,
            rooms_fetch_inflight: false,
            rooms_fetch_again: false,
            rooms_fetch_had_selection: false,
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
            rooms_without_derived_title: HashSet::new(),
            members_workers: Arc::new(Semaphore::new(MEMBERS_WORKERS)),
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

    /// Wire up the channel the main loop drains for completed startup stages
    /// and coalesced room refreshes (#189).
    pub(crate) fn set_bootstrap_sender(&mut self, tx: mpsc::UnboundedSender<BootstrapOutcome>) {
        self.bootstrap_tx = Some(tx);
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
                // Colors, density, time format and highlight feed the message
                // layout but are too expensive to hash on every tick, so the
                // layout digest keys on this counter instead. This is the only
                // place they change (pane-width tweaks move `messages.width`,
                // which the digest hashes directly). See `app::layout_cache`.
                self.config_generation = self.config_generation.wrapping_add(1);
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
                    || timeline::contains_ascii_case_insensitive(&self.room_list_title(room), q)
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
        self.input.backup_command_completion = None;
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
            Command::BackupEnable(target) => self.start_backup_enable(target),
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
            Command::Refresh => self.request_rooms_refresh(),
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

    /// This account's own Matrix user id, from the strongest source available.
    ///
    /// Three tiers, because no single one is reliable: `RoomDto` omits
    /// `account_user_id` on older servers, `own_senders` is only seeded once
    /// this client has seen one of its own events, and the account list is
    /// authoritative but indirect. Code that consults one tier alone gets `None`
    /// where another would have answered — which is how the reaction tally ended
    /// up with two disagreeing notions of "mine" (#220).
    pub(crate) fn own_user_id_for(&self, room: &RoomDto) -> Option<String> {
        room.account_user_id
            .clone()
            .or_else(|| self.live.own_senders.get(&room.account_id).cloned())
            .or_else(|| {
                // `account_user_id` is always present on AccountDto even when the
                // room DTO omits it (e.g. the server hasn't joined yet).
                self.accounts
                    .accounts
                    .iter()
                    .find(|a| a.account_id == room.account_id)
                    .map(|a| a.user_id.clone())
            })
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
        let sender = self.own_user_id_for(&room).unwrap_or_default();
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

#[cfg(test)]
mod tests;
