//! Room information for the `/whereami` popup (ADR 0114): the cached-state
//! reads Axon already serves, fetched off the event loop and rendered without
//! turning "Axon does not know" into "the room does not have one".

use chrono::{Local, TimeZone};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use unicode_width::UnicodeWidthChar;

use super::render::format_time;
use super::{App, Mode, PopupKind, RoomKey};
use crate::api::{
    ApiError, AxonClient, MemberDto, MetadataSnapshot, RoomCreationContent, RoomDto, RoomInfoDto,
    RoomJoinRulesContent, RoomMetadataDto, RoomPowerLevelsContent, RoomUpgradeDto, SnapshotStatus,
};
use crate::config::{ColorScheme, TimeFormat};

/// The four reads behind one `/whereami`: metadata, counts, upgrade, members.
const ROOM_INFO_READS: u8 = 4;

/// A pasted Matrix ID or display name fits. Anything longer cannot be a
/// substring of a field this short, and a huge paste must not be stored.
const MEMBER_FILTER_MAX_CHARS: usize = 1024;

/// One read's last good value, kept when a later refresh fails so the popup
/// does not go blank over a transient error.
#[derive(Debug)]
pub(crate) struct Part<T> {
    pub(crate) value: Option<T>,
    /// How the newest read failed; cleared while a retry is in flight.
    pub(crate) failure: Option<ReadFailure>,
}

/// Why a read produced nothing. The popup words these differently: retrying
/// helps one and not the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadFailure {
    /// HTTP 404: this axon predates the endpoint.
    Unsupported,
    /// Transport error, timeout, oversized or undecodable response.
    Failed,
}

impl From<ApiError> for ReadFailure {
    fn from(err: ApiError) -> Self {
        match err.is_not_found() {
            true => ReadFailure::Unsupported,
            false => ReadFailure::Failed,
        }
    }
}

impl<T> Default for Part<T> {
    fn default() -> Self {
        Self {
            value: None,
            failure: None,
        }
    }
}

impl<T> Part<T> {
    fn settle(&mut self, result: Result<T, ReadFailure>) {
        match result {
            Ok(value) => {
                self.value = Some(value);
                self.failure = None;
            }
            Err(failure) => self.failure = Some(failure),
        }
    }
}

#[derive(Debug)]
pub(crate) struct RoomInfoState {
    pub(crate) key: RoomKey,
    generation: u64,
    in_flight: u8,
    pub(crate) metadata: Part<RoomMetadataDto>,
    pub(crate) info: Part<RoomInfoDto>,
    pub(crate) upgrade: Part<RoomUpgradeDto>,
    pub(crate) members: Part<Vec<MemberDto>>,
    /// Member section rendered when a read settles. The frame wraps these
    /// lines; it does not sort or format the roster again.
    pub(crate) member_lines: Vec<String>,
    /// Local substring filter over loaded members. Empty shows the full list.
    /// Kept across a same-room refresh so a later poll does not drop it.
    pub(crate) member_filter: String,
    /// The filter prompt is taking keystrokes. Scroll keys still move the popup.
    pub(crate) member_filter_editing: bool,
}

/// Room information for the one room `/whereami` last showed. Holding a single
/// room bounds this cache without an eviction policy; reopening the same room
/// shows its earlier data while the refresh is in flight.
#[derive(Debug, Default)]
pub(crate) struct RoomInfoCache {
    pub(crate) state: Option<RoomInfoState>,
    generation: u64,
    /// The newest batch of reads. Aborted before another batch starts, so
    /// moving between rooms on a slow axon cannot pile up open requests: the
    /// generation check alone would drop their results but leave them running.
    reads: Vec<AbortHandle>,
    /// `None` until the main loop wires the channel (and in unit tests).
    pub(crate) tx: Option<mpsc::UnboundedSender<RoomInfoOutcome>>,
}

impl RoomInfoCache {
    fn abort_reads(&mut self) {
        for read in self.reads.drain(..) {
            read.abort();
        }
    }
}

#[derive(Debug)]
pub(crate) enum RoomInfoPart {
    /// Boxed: eight snapshots dwarf the other reads.
    Metadata(Result<Box<RoomMetadataDto>, ReadFailure>),
    Info(Result<RoomInfoDto, ReadFailure>),
    Upgrade(Result<RoomUpgradeDto, ReadFailure>),
    Members(Result<Vec<MemberDto>, ReadFailure>),
}

/// One completed read.
#[derive(Debug)]
pub(crate) struct RoomInfoOutcome {
    pub(crate) key: RoomKey,
    pub(crate) generation: u64,
    pub(crate) part: RoomInfoPart,
}

impl App {
    /// Start the room-information reads for `key` unless they are already in
    /// flight. Returns immediately; results land through
    /// [`App::apply_room_info_outcome`].
    pub(crate) fn request_room_info(&mut self, key: RoomKey) {
        let Some(tx) = self.room_info.tx.clone() else {
            return;
        };
        let cache = &mut self.room_info;
        if cache
            .state
            .as_ref()
            .is_some_and(|state| state.key == key && state.in_flight > 0)
        {
            return;
        }
        cache.abort_reads();
        cache.generation += 1;
        let generation = cache.generation;
        match cache.state.as_mut().filter(|state| state.key == key) {
            Some(state) => {
                state.generation = generation;
                state.in_flight = ROOM_INFO_READS;
                // A retry is pending again: say "loading", not the old failure.
                state.metadata.failure = None;
                state.info.failure = None;
                state.upgrade.failure = None;
                state.members.failure = None;
            }
            None => {
                cache.state = Some(RoomInfoState {
                    key: key.clone(),
                    generation,
                    in_flight: ROOM_INFO_READS,
                    metadata: Part::default(),
                    info: Part::default(),
                    upgrade: Part::default(),
                    members: Part::default(),
                    member_lines: Vec::new(),
                    member_filter: String::new(),
                    member_filter_editing: false,
                });
            }
        }

        // One task per read, so a slow one does not hold back the others.
        let mut reads = Vec::with_capacity(usize::from(ROOM_INFO_READS));
        let mut spawn_read = |read: fn(AxonClient, RoomKey) -> ReadFuture| {
            let (client, tx, key) = (self.client.clone(), tx.clone(), key.clone());
            let task = tokio::spawn(async move {
                let part = read(client, key.clone()).await;
                let _ = tx.send(RoomInfoOutcome {
                    key,
                    generation,
                    part,
                });
            });
            reads.push(task.abort_handle());
        };
        spawn_read(|client, key| {
            Box::pin(async move {
                let read = client.room_metadata(key.account_id, &key.room_id).await;
                RoomInfoPart::Metadata(read.map(Box::new).map_err(ReadFailure::from))
            })
        });
        spawn_read(|client, key| {
            Box::pin(async move {
                let read = client.room_info(key.account_id, &key.room_id).await;
                RoomInfoPart::Info(read.map_err(ReadFailure::from))
            })
        });
        spawn_read(|client, key| {
            Box::pin(async move {
                let read = client.room_upgrade(key.account_id, &key.room_id).await;
                RoomInfoPart::Upgrade(read.map_err(ReadFailure::from))
            })
        });
        spawn_read(|client, key| {
            Box::pin(async move {
                let read = client
                    .room_members_bounded(key.account_id, &key.room_id)
                    .await;
                RoomInfoPart::Members(read.map_err(ReadFailure::from))
            })
        });
        self.room_info.reads = reads;
        if let Some(state) = self.room_info.state.as_mut() {
            store_member_lines(state);
        }
    }

    /// Apply one completed read. A result for a room the popup has moved on
    /// from, or from a superseded request, is dropped.
    pub(crate) fn apply_room_info_outcome(&mut self, outcome: RoomInfoOutcome) {
        let Some(state) = self.room_info.state.as_mut() else {
            return;
        };
        if state.key != outcome.key || state.generation != outcome.generation {
            return;
        }
        state.in_flight = state.in_flight.saturating_sub(1);
        match outcome.part {
            RoomInfoPart::Metadata(result) => {
                state.metadata.settle(result.map(|metadata| *metadata))
            }
            RoomInfoPart::Info(result) => state.info.settle(result),
            RoomInfoPart::Upgrade(result) => state.upgrade.settle(result),
            RoomInfoPart::Members(result) => state.members.settle(result),
        }
        store_member_lines(state);
    }

    fn selected_room_key(&self) -> Option<RoomKey> {
        self.selected_room().map(RoomKey::from)
    }

    /// Room information for the room on screen, when it is the cached room.
    pub(crate) fn room_info_for_selected(&self) -> Option<&RoomInfoState> {
        let key = self.selected_room_key()?;
        self.room_info
            .state
            .as_ref()
            .filter(|state| state.key == key)
    }

    fn room_info_for_selected_mut(&mut self) -> Option<&mut RoomInfoState> {
        let key = self.selected_room_key()?;
        self.room_info
            .state
            .as_mut()
            .filter(|state| state.key == key)
    }

    /// The prompt is only live inside the room-info popup for the selected room.
    fn member_filter_prompt_is_live(&self) -> bool {
        matches!(self.mode, Mode::Popup(PopupKind::RoomInfo)) && self.member_filter_is_editing()
    }

    /// Stop taking keystrokes. The query stays, so the list stays narrowed.
    pub(crate) fn end_member_filter_prompt(&mut self) {
        if let Some(state) = self.room_info.state.as_mut() {
            state.member_filter_editing = false;
        }
    }

    /// Run `f` only while the prompt is on screen for the selected room.
    /// Any other situation clears a leftover editing flag and returns `None`.
    fn with_live_member_filter<T>(&mut self, f: impl FnOnce(&mut RoomInfoState) -> T) -> Option<T> {
        if !self.member_filter_prompt_is_live() {
            self.end_member_filter_prompt();
            return None;
        }
        let state = self.room_info_for_selected_mut()?;
        Some(f(state))
    }

    /// Open the member filter prompt. Returns false when this popup is not
    /// showing the selected room, so the key is not swallowed.
    pub(crate) fn begin_member_filter(&mut self) -> bool {
        if !matches!(self.mode, Mode::Popup(PopupKind::RoomInfo)) {
            return false;
        }
        let Some(state) = self.room_info_for_selected_mut() else {
            self.end_member_filter_prompt();
            return false;
        };
        state.member_filter_editing = true;
        true
    }

    pub(crate) fn member_filter_is_editing(&self) -> bool {
        self.room_info_for_selected()
            .is_some_and(|state| state.member_filter_editing)
    }

    pub(crate) fn push_member_filter_char(&mut self, ch: char) {
        let changed = self.with_live_member_filter(|state| {
            let before = state.member_filter.len();
            append_member_filter(state, std::iter::once(ch));
            state.member_filter.len() != before
        });
        if changed == Some(true) {
            self.reveal_room_info_members = true;
        }
    }

    pub(crate) fn pop_member_filter_char(&mut self) {
        let changed = self.with_live_member_filter(|state| {
            let before = state.member_filter.len();
            state.member_filter.pop();
            if state.member_filter.len() != before {
                store_member_lines(state);
                true
            } else {
                false
            }
        });
        if changed == Some(true) {
            self.reveal_room_info_members = true;
        }
    }

    /// Append pasted text while the room-info prompt is open.
    /// Returns false when it is not, so the caller can try another buffer.
    pub(crate) fn paste_member_filter(&mut self, text: &str) -> bool {
        let changed = self.with_live_member_filter(|state| {
            let before = state.member_filter.len();
            append_member_filter(state, text.chars());
            state.member_filter.len() != before
        });
        if changed == Some(true) {
            self.reveal_room_info_members = true;
        }
        changed.is_some()
    }

    /// Leave the prompt. A query that is only whitespace is cleared.
    /// A real query stays, including its surrounding spaces.
    pub(crate) fn finish_member_filter_edit(&mut self) {
        let reveal = {
            let Some(state) = self.room_info_for_selected_mut() else {
                return;
            };
            if !state.member_filter_editing {
                return;
            }
            state.member_filter_editing = false;
            if state.member_filter.trim().is_empty() {
                state.member_filter.clear();
                store_member_lines(state);
                true
            } else {
                false
            }
        };
        if reveal {
            self.reveal_room_info_members = true;
        }
    }

    /// Drop the filter and show every loaded member again.
    pub(crate) fn clear_member_filter(&mut self) {
        let cleared = {
            let Some(state) = self.room_info_for_selected_mut() else {
                return;
            };
            if state.member_filter.is_empty() && !state.member_filter_editing {
                return;
            }
            state.member_filter.clear();
            state.member_filter_editing = false;
            store_member_lines(state);
            true
        };
        if cleared {
            self.reveal_room_info_members = true;
        }
    }

    pub(crate) fn prune_room_info(&mut self, key: &RoomKey) {
        if self
            .room_info
            .state
            .as_ref()
            .is_some_and(|state| &state.key == key)
        {
            self.room_info.abort_reads();
            self.room_info.state = None;
        }
    }
}

type ReadFuture = std::pin::Pin<Box<dyn std::future::Future<Output = RoomInfoPart> + Send>>;

/// What the popup can say about one read.
enum View<'a, T> {
    Loaded(&'a T),
    Loading,
    Failed,
    Unsupported,
    NotRequested,
}

impl<T> View<'_, T> {
    fn pending_phrase(&self) -> &'static str {
        match self {
            View::Loaded(_) => "",
            View::Loading => "loading…",
            View::Failed => "unavailable (request failed)",
            View::Unsupported => "unavailable (this axon server does not provide it)",
            View::NotRequested => "not loaded",
        }
    }
}

fn view<'a, T>(
    state: Option<&'a RoomInfoState>,
    part: fn(&RoomInfoState) -> &Part<T>,
) -> View<'a, T> {
    let Some(state) = state else {
        return View::NotRequested;
    };
    let part = part(state);
    match &part.value {
        Some(value) => View::Loaded(value),
        None if part.failure == Some(ReadFailure::Unsupported) => View::Unsupported,
        None if part.failure.is_some() => View::Failed,
        None if state.in_flight > 0 => View::Loading,
        None => View::NotRequested,
    }
}

/// Wording for a snapshot that carries no content. `unknown` is never an
/// "unset": Axon simply has no cached tuple (ADR 0111).
fn no_content_phrase<T>(snapshot: &MetadataSnapshot<T>) -> &'static str {
    if snapshot.redacted == Some(true) {
        return "redacted";
    }
    match snapshot.status {
        SnapshotStatus::Unknown => "unknown (not cached)",
        SnapshotStatus::Unavailable => "unavailable (content not retained)",
        SnapshotStatus::Invalid => "unavailable (malformed state)",
        SnapshotStatus::TooLarge => "unavailable (state too large)",
        SnapshotStatus::Unrecognized => "unavailable (unrecognized status)",
        SnapshotStatus::Available | SnapshotStatus::Partial => "unavailable",
    }
}

/// Suffix for a line built from content the homeserver had already redacted:
/// room-version-protected fields survive, so the content is real but pruned.
fn redaction_note<T>(snapshot: &MetadataSnapshot<T>) -> &'static str {
    if snapshot.redacted == Some(true) {
        " (redacted; retained fields shown)"
    } else {
        ""
    }
}

fn withheld<T>(snapshot: &MetadataSnapshot<T>, path: &str) -> bool {
    snapshot.invalid_fields.iter().any(|field| field == path)
}

/// One optional scalar inside available content: its value, `unset` when the
/// event omitted it, or a marker when the server withheld a malformed value.
/// No Matrix default is substituted for a missing or withheld field.
fn scalar<T>(
    snapshot: &MetadataSnapshot<T>,
    path: &str,
    value: Option<String>,
    unset: &str,
) -> String {
    match value {
        Some(value) => value,
        None if withheld(snapshot, path) => "withheld (malformed)".to_owned(),
        None => unset.to_owned(),
    }
}

/// Like [`scalar`], for a snapshot whose only interesting field is one string.
fn single_field<T>(
    snapshot: &MetadataSnapshot<T>,
    path: &str,
    value: impl Fn(&T) -> Option<&String>,
) -> String {
    match &snapshot.content {
        Some(content) => format!(
            "{}{}",
            scalar(snapshot, path, value(content).cloned(), "not set"),
            redaction_note(snapshot)
        ),
        None => no_content_phrase(snapshot).to_owned(),
    }
}

fn format_datetime(ts: i64, time_format: TimeFormat) -> String {
    let date = Local
        .timestamp_millis_opt(ts)
        .single()
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "----------".to_owned());
    format!("{date} {}", format_time(ts, time_format))
}

fn format_period_ms(ms: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (86_400_000, "d"),
        (3_600_000, "h"),
        (60_000, "min"),
        (1000, "s"),
    ];
    UNITS
        .iter()
        .find(|(unit, _)| ms >= *unit && ms.is_multiple_of(*unit))
        .map(|(unit, label)| format!("{}{label}", ms / unit))
        .unwrap_or_else(|| format!("{ms}ms"))
}

fn alias_lines(room: &RoomDto, metadata: &View<'_, RoomMetadataDto>) -> Vec<String> {
    let summary_alias = room.canonical_alias.as_deref();
    let View::Loaded(metadata) = metadata else {
        return vec![
            format!(
                "Canonical alias: {}",
                summary_alias.unwrap_or("none in room summary")
            ),
            format!("Advertised aliases: {}", metadata.pending_phrase()),
        ];
    };
    let snapshot = &metadata.aliases;
    let Some(content) = &snapshot.content else {
        let phrase = no_content_phrase(snapshot);
        return vec![
            format!("Canonical alias: {}", summary_alias.unwrap_or(phrase)),
            format!("Advertised aliases: {phrase}"),
        ];
    };
    let canonical = scalar(snapshot, "alias", content.alias.clone(), "none");
    let mut advertised = match &content.alt_aliases {
        Some(aliases) if !aliases.is_empty() => aliases.join(", "),
        Some(_) => "none".to_owned(),
        None if withheld(snapshot, "alt_aliases") => "withheld (malformed)".to_owned(),
        None => "none".to_owned(),
    };
    if withheld(snapshot, "alt_aliases[]") {
        advertised.push_str(" (some entries withheld)");
    }
    vec![
        format!("Canonical alias: {canonical}{}", redaction_note(snapshot)),
        format!("Advertised aliases: {advertised}"),
    ]
}

fn member_count_line(info: &View<'_, RoomInfoDto>, time_format: TimeFormat) -> String {
    let View::Loaded(info) = info else {
        return format!("Members: {}", info.pending_phrase());
    };
    match &info.member_counts {
        Some(Some(counts)) => format!(
            // `observed_at` is when the pair last changed locally, not a
            // confirmation time, so do not word it as freshness.
            "Members: {} joined, {} invited (last changed {})",
            counts.joined,
            counts.invited,
            format_datetime(counts.observed_at, time_format)
        ),
        // Null is "not observed yet", never zero.
        Some(None) => "Members: unknown (not yet observed)".to_owned(),
        // No field at all: this server will never observe one.
        None => "Members: unavailable (this axon server does not report member counts)".to_owned(),
    }
}

fn encryption_line(metadata: &RoomMetadataDto) -> String {
    let snapshot = &metadata.encryption;
    let Some(content) = &snapshot.content else {
        // An absent tuple does not establish an unencrypted room.
        let phrase = match snapshot.status {
            SnapshotStatus::Unknown if snapshot.redacted != Some(true) => {
                "no encryption state cached"
            }
            _ => no_content_phrase(snapshot),
        };
        return format!("Encryption: {phrase}");
    };
    let mut line = format!(
        "Encryption: {}",
        scalar(
            snapshot,
            "algorithm",
            content.algorithm.clone(),
            "algorithm not set"
        )
    );
    let mut rotation = Vec::new();
    if let Some(ms) = content.rotation_period_ms {
        rotation.push(format_period_ms(ms));
    }
    if let Some(messages) = content.rotation_period_msgs {
        rotation.push(format!("{messages} messages"));
    }
    if !rotation.is_empty() {
        line.push_str(&format!("; key rotation every {}", rotation.join(" or ")));
    }
    if withheld(snapshot, "rotation_period_ms") || withheld(snapshot, "rotation_period_msgs") {
        line.push_str(" (some rotation settings withheld)");
    }
    line.push_str(redaction_note(snapshot));
    line
}

fn join_rule_line(snapshot: &MetadataSnapshot<RoomJoinRulesContent>) -> String {
    let Some(content) = &snapshot.content else {
        return format!("Join rule: {}", no_content_phrase(snapshot));
    };
    let mut line = format!(
        "Join rule: {}",
        scalar(snapshot, "join_rule", content.join_rule.clone(), "not set")
    );
    let mut conditions: Vec<String> = content
        .allow
        .iter()
        .flatten()
        .map(|condition| match &condition.room_id {
            Some(room_id) if condition.condition_type == "m.room_membership" => {
                format!("members of {room_id}")
            }
            _ => condition.condition_type.clone(),
        })
        .collect();
    if withheld(snapshot, "allow") || withheld(snapshot, "allow[]") {
        conditions.push("some conditions withheld".to_owned());
    }
    if !conditions.is_empty() {
        line.push_str(&format!(" ({})", conditions.join("; ")));
    }
    line.push_str(redaction_note(snapshot));
    line
}

fn creation_lines(
    snapshot: &MetadataSnapshot<RoomCreationContent>,
    time_format: TimeFormat,
) -> Vec<String> {
    let Some(content) = &snapshot.content else {
        let phrase = no_content_phrase(snapshot);
        return vec![
            format!("Room type: {phrase}"),
            format!("Room version: {phrase}"),
            format!("Creator: {phrase}"),
        ];
    };
    // Versions that drop `creator` from the content identify the creator as
    // the create event's sender.
    let creator = content.creator.clone().or_else(|| snapshot.sender.clone());
    let mut creator_line = format!(
        "Creator: {}",
        scalar(snapshot, "creator", creator, "not recorded")
    );
    match content.additional_creators.as_deref() {
        Some(others) if !others.is_empty() => {
            creator_line.push_str(&format!("; additional creators: {}", others.join(", ")));
        }
        _ => {}
    }
    if withheld(snapshot, "additional_creators") || withheld(snapshot, "additional_creators[]") {
        creator_line.push_str(" (some creators withheld)");
    }
    let federation = match content.federate {
        Some(true) => "enabled".to_owned(),
        Some(false) => "disabled".to_owned(),
        None => scalar(snapshot, "m.federate", None, "not specified"),
    };
    let mut lines = vec![
        format!(
            "Room type: {}{}",
            scalar(
                snapshot,
                "type",
                content.room_type.clone(),
                "none (ordinary room)"
            ),
            redaction_note(snapshot)
        ),
        format!(
            "Room version: {}",
            scalar(
                snapshot,
                "room_version",
                content.room_version.clone(),
                "not specified"
            )
        ),
        creator_line,
    ];
    if let Some(ts) = snapshot.origin_ts {
        lines.push(format!("Created: {}", format_datetime(ts, time_format)));
    }
    lines.push(format!("Federation: {federation}"));
    lines
}

fn upgrade_lines(
    creation: Option<&MetadataSnapshot<RoomCreationContent>>,
    upgrade: &View<'_, RoomUpgradeDto>,
) -> Vec<String> {
    let loaded = match upgrade {
        View::Loaded(upgrade) => Some(*upgrade),
        _ => None,
    };
    let predecessor = creation
        .and_then(|snapshot| snapshot.content.as_ref())
        .and_then(|content| content.predecessor.as_ref())
        .map(|predecessor| predecessor.room_id.clone())
        .or_else(|| loaded.and_then(|upgrade| upgrade.upgraded_from.clone()));
    let predecessor = match (predecessor, creation) {
        (Some(room_id), _) => room_id,
        (None, Some(snapshot)) if snapshot.content.is_some() => {
            scalar(snapshot, "predecessor", None, "none")
        }
        (None, Some(snapshot)) => no_content_phrase(snapshot).to_owned(),
        // `/upgrade` cannot tell "no predecessor" from "none cached" either.
        (None, None) if loaded.is_some() => "none known".to_owned(),
        (None, None) => upgrade.pending_phrase().to_owned(),
    };
    let successor = match loaded {
        Some(upgrade) => upgrade
            .tombstoned_to
            .clone()
            // The read cannot tell "no tombstone" from "none cached".
            .unwrap_or_else(|| "none known".to_owned()),
        None => upgrade.pending_phrase().to_owned(),
    };
    vec![
        format!("Upgraded from: {predecessor}"),
        format!("Replaced by: {successor}"),
    ]
}

fn level(
    snapshot: &MetadataSnapshot<RoomPowerLevelsContent>,
    path: &str,
    value: Option<i64>,
) -> String {
    format!(
        "{path} {}",
        scalar(
            snapshot,
            path,
            value.map(|value| value.to_string()),
            "unset"
        )
    )
}

fn level_map_lines(
    snapshot: &MetadataSnapshot<RoomPowerLevelsContent>,
    label: &str,
    path: &str,
    map: Option<&std::collections::BTreeMap<String, i64>>,
) -> Vec<String> {
    let entry_withheld = withheld(snapshot, &format!("{path}.*"));
    let note = if entry_withheld {
        " (some entries withheld)"
    } else {
        ""
    };
    match map {
        Some(map) if !map.is_empty() => std::iter::once(format!("  {label}:{note}"))
            .chain(map.iter().map(|(key, value)| format!("    {key}  {value}")))
            .collect(),
        Some(_) => vec![format!("  {label}: none configured{note}")],
        None if withheld(snapshot, path) => vec![format!("  {label}: withheld (malformed)")],
        None => vec![format!("  {label}: none configured")],
    }
}

fn power_level_lines(metadata: &RoomMetadataDto) -> Vec<String> {
    let snapshot = &metadata.power_levels;
    // Stored values only: defaults and room-version rules are not applied, so
    // these must not be read as who may do what.
    let heading = "Power levels (configured values, not resolved permissions)";
    let Some(content) = &snapshot.content else {
        return vec![format!("{heading}: {}", no_content_phrase(snapshot))];
    };
    let mut lines = vec![
        format!("{heading}:{}", redaction_note(snapshot)),
        format!(
            "  {}, {}, {}, {}",
            level(snapshot, "ban", content.ban),
            level(snapshot, "kick", content.kick),
            level(snapshot, "invite", content.invite),
            level(snapshot, "redact", content.redact)
        ),
        format!(
            "  {}, {}, {}",
            level(snapshot, "events_default", content.events_default),
            level(snapshot, "state_default", content.state_default),
            level(snapshot, "users_default", content.users_default)
        ),
    ];
    let room_notification = content
        .notifications
        .as_ref()
        .and_then(|levels| levels.room);
    if room_notification.is_some() || withheld(snapshot, "notifications.room") {
        lines.push(format!(
            "  {}",
            level(snapshot, "notifications.room", room_notification)
        ));
    }
    lines.extend(level_map_lines(
        snapshot,
        "Users",
        "users",
        content.users.as_ref(),
    ));
    lines.extend(level_map_lines(
        snapshot,
        "Event overrides",
        "events",
        content.events.as_ref(),
    ));
    // From room version 12 creators outrank every level and cannot appear in
    // `users` (#324); without this an empty map reads as "nobody is an admin".
    let privileged_creators = metadata
        .creation
        .content
        .as_ref()
        .and_then(|creation| creation.room_version.as_deref())
        .and_then(|version| version.parse::<u32>().ok())
        .is_some_and(|version| version >= 12);
    if privileged_creators {
        lines.push(
            "  Room version 12+: creators outrank these levels and are not listed under Users."
                .to_owned(),
        );
    }
    lines
}

fn server_acl_lines(metadata: &RoomMetadataDto) -> Vec<String> {
    let snapshot = &metadata.server_acl;
    // Not in the sync defaults, so usually unknown; say nothing until a tuple
    // exists rather than adding a permanent "unknown" line to every room.
    if snapshot.status == SnapshotStatus::Unknown && snapshot.redacted != Some(true) {
        return Vec::new();
    }
    let Some(content) = &snapshot.content else {
        return vec![format!("Server ACL: {}", no_content_phrase(snapshot))];
    };
    let count = |path: &str, list: Option<&Vec<String>>| match list {
        Some(list) if withheld(snapshot, &format!("{path}[]")) => {
            format!("{} (some entries withheld)", list.len())
        }
        Some(list) => list.len().to_string(),
        None => scalar(snapshot, path, None, "unset"),
    };
    let ip_literals = match content.allow_ip_literals {
        Some(true) => "allowed".to_owned(),
        Some(false) => "denied".to_owned(),
        None => scalar(snapshot, "allow_ip_literals", None, "not specified"),
    };
    vec![format!(
        "Server ACL: allow {}, deny {}, IP literals {ip_literals}{}",
        count("allow", content.allow.as_ref()),
        count("deny", content.deny.as_ref()),
        redaction_note(snapshot)
    )]
}

/// The lines `/whereami` adds below the room summary: everything read from
/// `/metadata`, `/info`, and `/upgrade`. The member projection is
/// [`member_list_lines`]; a failed refresh is flagged by
/// [`refresh_failed_with_stale_data`] after both sections.
pub(crate) fn detail_lines(
    room: &RoomDto,
    state: Option<&RoomInfoState>,
    time_format: TimeFormat,
) -> Vec<String> {
    // The cache holds one room; ignore it if the selection has moved on.
    let state = state.filter(|state| state.key == RoomKey::from(room));
    let metadata = view(state, |state| &state.metadata);
    let info = view(state, |state| &state.info);
    let upgrade = view(state, |state| &state.upgrade);

    let mut lines = alias_lines(room, &metadata);
    lines.push(member_count_line(&info, time_format));
    match &metadata {
        View::Loaded(metadata) => {
            lines.push(encryption_line(metadata));
            lines.push(join_rule_line(&metadata.join_rules));
            lines.push(format!(
                "History visibility: {}",
                single_field(
                    &metadata.history_visibility,
                    "history_visibility",
                    |content| { content.history_visibility.as_ref() }
                )
            ));
            lines.push(format!(
                "Guest access: {}",
                single_field(&metadata.guest_access, "guest_access", |content| {
                    content.guest_access.as_ref()
                })
            ));
            lines.extend(creation_lines(&metadata.creation, time_format));
            lines.extend(upgrade_lines(Some(&metadata.creation), &upgrade));
            lines.extend(server_acl_lines(metadata));
            lines.push(String::new());
            lines.extend(power_level_lines(metadata));
        }
        pending => {
            lines.push(format!(
                "Encryption, access, and room details: {}",
                pending.pending_phrase()
            ));
            lines.extend(upgrade_lines(None, &upgrade));
        }
    }
    lines
}

fn part_holds_stale<T>(part: &Part<T>) -> bool {
    part.failure.is_some() && part.value.is_some()
}

/// A later read failed after an earlier one had data. The popup keeps that
/// data and says it may be out of date. `state` is already the room on screen.
pub(crate) fn refresh_failed_with_stale_data(state: Option<&RoomInfoState>) -> bool {
    let Some(state) = state else {
        return false;
    };
    part_holds_stale(&state.metadata)
        || part_holds_stale(&state.info)
        || part_holds_stale(&state.upgrade)
        || part_holds_stale(&state.members)
}

fn store_member_lines(state: &mut RoomInfoState) {
    let lines = render_member_section(state);
    state.member_lines = lines;
}

/// Keep control characters out of the prompt and stop at the cap.
fn append_member_filter(state: &mut RoomInfoState, chars: impl IntoIterator<Item = char>) {
    let mut count = state.member_filter.chars().count();
    for ch in chars {
        if ch.is_control() {
            continue;
        }
        if count >= MEMBER_FILTER_MAX_CHARS {
            break;
        }
        state.member_filter.push(ch);
        count += 1;
    }
    store_member_lines(state);
}

/// The `/members` section. Every loaded row is rendered; the popup scrolls.
/// The heading never calls the list complete: the projection is lazy, and a
/// joined count larger than the list is the only proof of a gap.
#[cfg(test)]
fn member_list_lines(room: &RoomDto, state: Option<&RoomInfoState>) -> Vec<String> {
    let Some(state) = state.filter(|state| state.key == RoomKey::from(room)) else {
        return vec!["Member list: not loaded".to_owned()];
    };
    render_member_section(state)
}

fn render_member_section(state: &RoomInfoState) -> Vec<String> {
    let query = state.member_filter.trim();
    let filtering = !query.is_empty();
    let state = Some(state);
    let members = view(state, |state| &state.members);
    let View::Loaded(members) = members else {
        return vec![format!("Member list: {}", members.pending_phrase())];
    };
    let joined = match view(state, |state| &state.info) {
        View::Loaded(info) => match &info.member_counts {
            Some(Some(counts)) => Some(counts.joined),
            Some(None) | None => None,
        },
        _ => None,
    };
    let metadata = match view(state, |state| &state.metadata) {
        View::Loaded(metadata) => Some(metadata),
        _ => None,
    };
    let matched: Vec<&MemberDto> = if filtering {
        members
            .iter()
            .filter(|member| member_matches(member, query))
            .collect()
    } else {
        Vec::new()
    };
    let mut lines = vec![member_heading(joined, members.len())];
    // An empty projection has nobody to match. "none loaded" is the whole story.
    if members.is_empty() {
        lines.push("  none loaded".to_owned());
        return lines;
    }
    if filtering {
        lines.push(member_match_line(matched.len(), members.len(), joined));
    }
    if filtering {
        if matched.is_empty() {
            lines.push("  none match".to_owned());
            return lines;
        }
        lines.extend(grouped_member_lines(matched, metadata));
        return lines;
    }
    lines.extend(grouped_member_lines(members.iter(), metadata));
    lines
}

/// `K of N loaded members match`, plus the partial-list warning only when the
/// joined count proves the loaded roster is short.
fn member_match_line(matches: usize, loaded: usize, joined: Option<i64>) -> String {
    let loaded_count = i64::try_from(loaded).unwrap_or(i64::MAX);
    match joined {
        Some(joined) if joined > loaded_count => format!(
            "{matches} of {loaded} loaded members match (room has more members than are loaded)"
        ),
        _ => format!("{matches} of {loaded} loaded members match"),
    }
}

/// Case-insensitive substring of the display name or Matrix ID.
/// A blank name is not the on-screen word "unknown", and power text is not a field.
fn member_matches(member: &MemberDto, query: &str) -> bool {
    let folded_query = fold_match_key(query);
    if folded_query.is_empty() {
        return false;
    }
    let name_matches = member
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .is_some_and(|name| fold_match_key(name).contains(&folded_query));
    name_matches || fold_match_key(&member.user_id).contains(&folded_query)
}

/// Unicode lowercase, then Latin accents folded to their base letter, so
/// "ärger" matches "Ärger" and "jose" matches "José".
/// Combining marks produced by lowercase (Turkish İ) are dropped.
fn fold_match_key(value: &str) -> String {
    let mut folded = String::new();
    for ch in value.chars().flat_map(char::to_lowercase) {
        if is_combining_mark(ch) {
            continue;
        }
        push_folded_letter(&mut folded, ch);
    }
    folded
}

fn is_combining_mark(ch: char) -> bool {
    matches!(
        ch,
        '\u{0300}'..='\u{036f}'
            | '\u{1ab0}'..='\u{1aff}'
            | '\u{1dc0}'..='\u{1dff}'
            | '\u{20d0}'..='\u{20ff}'
            | '\u{fe20}'..='\u{fe2f}'
    )
}

fn push_folded_letter(out: &mut String, ch: char) {
    match ch {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => out.push('a'),
        'æ' => out.push_str("ae"),
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => out.push('c'),
        'ď' | 'đ' | 'ð' => out.push('d'),
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => out.push('e'),
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => out.push('g'),
        'ĥ' | 'ħ' => out.push('h'),
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => out.push('i'),
        'ĵ' => out.push('j'),
        'ķ' => out.push('k'),
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => out.push('l'),
        'ñ' | 'ń' | 'ņ' | 'ň' | 'ŋ' => out.push('n'),
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ō' | 'ŏ' | 'ő' | 'ø' => out.push('o'),
        'œ' => out.push_str("oe"),
        'ŕ' | 'ŗ' | 'ř' => out.push('r'),
        'ś' | 'ŝ' | 'ş' | 'š' | 'ș' => out.push('s'),
        'ß' => out.push_str("ss"),
        'ţ' | 'ť' | 'ŧ' | 'ț' => out.push('t'),
        'þ' => out.push_str("th"),
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => out.push('u'),
        'ŵ' => out.push('w'),
        'ý' | 'ÿ' | 'ŷ' => out.push('y'),
        'ź' | 'ż' | 'ž' => out.push('z'),
        other => out.push(other),
    }
}

fn member_heading(joined: Option<i64>, loaded: usize) -> String {
    let loaded_count = i64::try_from(loaded).unwrap_or(i64::MAX);
    match joined {
        Some(joined) if joined > loaded_count => {
            format!("Members ({loaded} loaded of {joined} joined)")
        }
        Some(_) => format!("Members ({loaded} loaded)"),
        None => format!("Members ({loaded} loaded; joined count unknown)"),
    }
}

/// Membership values the server documents, in the order the popup shows them.
const MEMBER_GROUPS: [(&str, &str); 4] = [
    ("join", "Joined"),
    ("invite", "Invited"),
    ("ban", "Banned"),
    ("leave", "Left"),
];

fn grouped_member_lines<'a>(
    members: impl IntoIterator<Item = &'a MemberDto>,
    metadata: Option<&RoomMetadataDto>,
) -> Vec<String> {
    let mut ordered: Vec<&MemberDto> = members.into_iter().collect();
    ordered.sort_by_cached_key(|member| (member_sort_name(member), member.user_id.clone()));

    let mut lines = Vec::new();
    for (membership, label) in MEMBER_GROUPS {
        push_member_group(
            &mut lines,
            label,
            ordered
                .iter()
                .copied()
                .filter(|member| member.membership == membership),
            metadata,
        );
    }
    let mut extras: std::collections::BTreeMap<&str, Vec<&MemberDto>> =
        std::collections::BTreeMap::new();
    for member in ordered {
        if !MEMBER_GROUPS
            .iter()
            .any(|(membership, _)| member.membership == *membership)
        {
            extras
                .entry(member.membership.as_str())
                .or_default()
                .push(member);
        }
    }
    for (membership, group) in extras {
        // An empty string would be a blank header. Name it instead of calling
        // the row `leave`.
        let label = if membership.is_empty() {
            "(unset membership)"
        } else {
            membership
        };
        push_member_group(&mut lines, label, group.into_iter(), metadata);
    }
    lines
}

fn push_member_group<'a>(
    lines: &mut Vec<String>,
    label: &str,
    members: impl Iterator<Item = &'a MemberDto>,
    metadata: Option<&RoomMetadataDto>,
) {
    let group: Vec<&MemberDto> = members.collect();
    if group.is_empty() {
        return;
    }
    lines.push(format!("  {label}"));
    for member in group {
        lines.push(format!(
            "    {}  {}  {}",
            shown_member_name(member),
            member.user_id,
            power_phrase(metadata, &member.user_id)
        ));
    }
}

/// Blank names sort by Matrix ID. They still render as `unknown`.
fn member_sort_name(member: &MemberDto) -> String {
    member
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(member.user_id.as_str())
        .to_ascii_lowercase()
}

fn shown_member_name(member: &MemberDto) -> &str {
    member
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("unknown")
}

/// A number only when this user has an entry in the stored `users` map.
/// `users_default` is not applied: a missing entry is "no per-user power
/// level", even when the room default is non-zero. A room-v12 creator is
/// not given a level.
fn power_phrase(metadata: Option<&RoomMetadataDto>, user_id: &str) -> String {
    let Some(metadata) = metadata else {
        return "configured power unknown".to_owned();
    };
    if is_privileged_creator(metadata, user_id) {
        return "creator (power not represented)".to_owned();
    }
    let snapshot = &metadata.power_levels;
    if !matches!(
        snapshot.status,
        SnapshotStatus::Available | SnapshotStatus::Partial
    ) {
        return "configured power unknown".to_owned();
    }
    if withheld(snapshot, "users") || withheld(snapshot, "users.*") {
        return "configured power unknown".to_owned();
    }
    let Some(content) = snapshot.content.as_ref() else {
        return "configured power unknown".to_owned();
    };
    match content.users.as_ref().and_then(|users| users.get(user_id)) {
        Some(level) => format!("configured power {level}"),
        None => "no per-user power level".to_owned(),
    }
}

fn is_privileged_creator(metadata: &RoomMetadataDto, user_id: &str) -> bool {
    let snapshot = &metadata.creation;
    if !matches!(
        snapshot.status,
        SnapshotStatus::Available | SnapshotStatus::Partial
    ) {
        return false;
    }
    let Some(content) = snapshot.content.as_ref() else {
        return false;
    };
    let Some(version) = content.room_version.as_deref() else {
        return false;
    };
    match version.parse::<u32>() {
        Ok(version) if version >= 12 => {}
        _ => return false,
    }
    let creator = content.creator.as_deref().or(snapshot.sender.as_deref());
    creator == Some(user_id)
        || content
            .additional_creators
            .as_ref()
            .is_some_and(|ids| ids.iter().any(|id| id == user_id))
}

/// Values that report a gap in what Axon knows rather than room data.
const GAP_PREFIXES: [&str; 8] = [
    "unknown",
    "unavailable",
    "loading",
    "not loaded",
    "no encryption state cached",
    "none known",
    "withheld",
    "redacted",
];

/// Summary rows whose values come from the room itself (a topic may well
/// begin "unavailable …"), so they are never dimmed as gaps.
const SUMMARY_LABELS: [&str; 9] = [
    "Name",
    "DM name",
    "Matrix ID",
    "Account ID",
    "Your Matrix ID",
    "Topic",
    "Avatar",
    "Last activity",
    "Last event",
];

/// Room state is written by other people. Keep control characters out of the
/// terminal and off the row count: whitespace ones become a space, the rest
/// a visible replacement.
fn printable(line: String) -> String {
    if !line.contains(char::is_control) {
        return line;
    }
    line.chars()
        .map(|ch| match ch {
            '\n' | '\r' | '\t' => ' ',
            ch if ch.is_control() => char::REPLACEMENT_CHARACTER,
            ch => ch,
        })
        .collect()
}

/// Colour one popup line: the field name in the heading colour, and a value
/// that only reports a gap dimmed, so real data stands out from both.
///
/// Only unindented `Label: value` rows and `Heading:` rows are split. Indented
/// rows are list entries whose text (a display name, say) may itself contain
/// a colon.
pub(crate) fn styled_line(line: String, colors: &ColorScheme) -> Line<'static> {
    let line = printable(line);
    let label_style = Style::default().fg(colors.selected_room);
    if line.starts_with(' ') {
        return Line::from(line);
    }
    let Some((label, value)) = line.split_once(": ") else {
        return match line.ends_with(':') {
            true => Line::from(Span::styled(line, label_style)),
            false => Line::from(line),
        };
    };
    let is_gap =
        !SUMMARY_LABELS.contains(&label) && GAP_PREFIXES.iter().any(|gap| value.starts_with(gap));
    let value_style = match is_gap {
        true => Style::default().fg(colors.input_hint),
        false => Style::default(),
    };
    Line::from(vec![
        Span::styled(format!("{label}: "), label_style),
        Span::styled(value.to_owned(), value_style),
    ])
}

/// Break one styled line into rows no wider than `width`, at a space where
/// the row has one and mid-word otherwise. Continuation rows hang two columns
/// inside the line's own indent, so a wrapped entry reads as one entry.
///
/// The popup pages by row, so a long list left as one logical line would be
/// skipped whole by a single Down, and its tail could never be scrolled into
/// view once it wrapped past the popup's height.
pub(crate) fn wrap_styled_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| span.content.chars().map(|ch| (ch, span.style)))
        .collect();
    let cell_width = |cells: &[(char, Style)]| -> usize {
        cells.iter().map(|(ch, _)| ch.width().unwrap_or(0)).sum()
    };
    let indent = cells.iter().take_while(|(ch, _)| *ch == ' ').count();
    // No hanging indent once it would leave nothing to write in.
    let hang = match indent + 2 < width {
        true => indent + 2,
        false => 0,
    };
    let hanging_row = || vec![(' ', Style::default()); hang];

    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut row: Vec<(char, Style)> = Vec::new();
    // Cells before this index are indentation, never a place to break.
    let mut text_start = indent;
    for (ch, style) in cells {
        let ch_width = ch.width().unwrap_or(0);
        if row.len() > text_start && cell_width(&row) + ch_width > width {
            let mut next = hanging_row();
            // Carry the unfinished word down, unless it would not fit there
            // either; the space that overflowed is the break itself.
            let word_start = row[text_start..]
                .iter()
                .rposition(|(cell, _)| *cell == ' ')
                .map(|space| text_start + space + 1)
                .filter(|start| ch != ' ' && hang + cell_width(&row[*start..]) + ch_width <= width);
            if let Some(start) = word_start {
                next.extend(row.split_off(start));
            }
            rows.push(std::mem::replace(&mut row, next));
            text_start = hang;
            if ch == ' ' {
                continue;
            }
        }
        row.push((ch, style));
    }
    rows.push(row);
    rows.into_iter()
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (ch, style) in row {
                match spans.last_mut() {
                    Some(span) if span.style == style => span.content.to_mut().push(ch),
                    _ => spans.push(Span::styled(ch.to_string(), style)),
                }
            }
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{RoomMemberCountsDto, RoomPredecessorContent};
    use crate::app::{Mode, PopupKind};
    use crate::config::{KeyBinding, TuiConfig};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;
    use serde_json::json;
    use uuid::Uuid;

    fn room() -> RoomDto {
        RoomDto {
            account_id: Uuid::nil(),
            account_user_id: Some("@me:example.com".to_owned()),
            room_id: "!room:example.com".to_owned(),
            name: Some("Ops".to_owned()),
            topic: None,
            avatar_url: None,
            display_name: None,
            canonical_alias: Some("#summary:example.com".to_owned()),
            room_type: None,
            last_activity_ts: 0,
            last_event_id: None,
            notification_count: 0,
            highlight_count: 0,
            tags: Vec::new(),
            is_direct: false,
        }
    }

    fn key() -> RoomKey {
        RoomKey::from(&room())
    }

    fn app() -> App {
        // A closed port: any request a test does spawn fails fast instead of
        // reaching a developer's local axon.
        let mut app = App::new(
            AxonClient::new("http://127.0.0.1:1".to_owned(), None),
            None,
            TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );
        app.rooms.rooms = vec![room()];
        app.rooms.selected = Some(0);
        app
    }

    fn loaded(
        metadata: Option<RoomMetadataDto>,
        info: Option<RoomInfoDto>,
        upgrade: Option<RoomUpgradeDto>,
    ) -> RoomInfoState {
        RoomInfoState {
            key: key(),
            generation: 0,
            in_flight: 0,
            metadata: Part {
                value: metadata,
                failure: None,
            },
            info: Part {
                value: info,
                failure: None,
            },
            upgrade: Part {
                value: upgrade,
                failure: None,
            },
            members: Part::default(),
            member_lines: Vec::new(),
            member_filter: String::new(),
            member_filter_editing: false,
        }
    }

    /// A snapshot as the server serializes it, including the fields this
    /// client does not read.
    fn snapshot(status: &str, content: serde_json::Value) -> serde_json::Value {
        json!({
            "status": status,
            "event_id": "$state:example.com",
            "sender": "@founder:example.com",
            "origin_ts": 1_700_000_000_000_i64,
            "redacted": null,
            "redaction_event_id": null,
            "invalid_fields": [],
            "content": content,
        })
    }

    fn unknown() -> serde_json::Value {
        json!({
            "status": "unknown", "event_id": null, "sender": null, "origin_ts": null,
            "redacted": null, "redaction_event_id": null, "invalid_fields": [], "content": null,
        })
    }

    fn full_metadata() -> RoomMetadataDto {
        serde_json::from_value(json!({
            "aliases": snapshot("available", json!({
                "alias": "#ops:example.com",
                "alt_aliases": ["#operations:example.com", "#ops:other.example"],
            })),
            "creation": snapshot("available", json!({
                "creator": null, "additional_creators": null, "room_version": "11",
                "federate": false, "room_type": "m.space",
                "predecessor": { "room_id": "!old:example.com", "event_id": null },
            })),
            "join_rules": snapshot("available", json!({
                "join_rule": "restricted",
                "allow": [
                    { "type": "m.room_membership", "room_id": "!space:example.com" },
                    { "type": "org.example.custom", "room_id": null },
                ],
            })),
            "encryption": snapshot("available", json!({
                "algorithm": "m.megolm.v1.aes-sha2",
                "rotation_period_ms": 604_800_000_u64,
                "rotation_period_msgs": 100,
            })),
            "power_levels": snapshot("available", json!({
                "ban": 50, "invite": 0, "kick": 50, "redact": null,
                "events_default": 0, "state_default": 50, "users_default": 0,
                "users": { "@admin:example.com": 100, "@mod:example.com": 50 },
                "events": { "m.room.name": 50 },
                "notifications": { "room": 50 },
            })),
            "server_acl": snapshot("available", json!({
                "allow_ip_literals": false, "allow": ["*"], "deny": ["bad.example", "worse.example"],
            })),
            "history_visibility": snapshot("available", json!({ "history_visibility": "shared" })),
            "guest_access": snapshot("available", json!({ "guest_access": "forbidden" })),
        }))
        .expect("the server's metadata shape must decode")
    }

    fn all_unknown() -> RoomMetadataDto {
        serde_json::from_value(json!({
            "aliases": unknown(), "creation": unknown(), "join_rules": unknown(),
            "encryption": unknown(), "power_levels": unknown(), "server_acl": unknown(),
            "history_visibility": unknown(), "guest_access": unknown(),
        }))
        .expect("unknown snapshots must decode")
    }

    fn lines(state: &RoomInfoState) -> Vec<String> {
        detail_lines(&room(), Some(state), TimeFormat::H24)
    }

    fn line<'a>(lines: &'a [String], prefix: &str) -> &'a str {
        lines
            .iter()
            .find(|line| line.starts_with(prefix))
            .unwrap_or_else(|| panic!("no line starting {prefix:?} in {lines:#?}"))
    }

    #[test]
    fn available_metadata_renders_every_detail() {
        let state = loaded(
            Some(full_metadata()),
            Some(RoomInfoDto {
                member_counts: Some(Some(RoomMemberCountsDto {
                    joined: 412,
                    invited: 3,
                    observed_at: 1_700_000_000_000,
                })),
            }),
            Some(RoomUpgradeDto {
                tombstoned_to: Some("!new:example.com".to_owned()),
                upgraded_from: None,
            }),
        );
        let lines = lines(&state);

        assert_eq!(
            line(&lines, "Canonical alias:"),
            "Canonical alias: #ops:example.com"
        );
        assert_eq!(
            line(&lines, "Advertised aliases:"),
            "Advertised aliases: #operations:example.com, #ops:other.example"
        );
        assert!(
            line(&lines, "Members:").starts_with("Members: 412 joined, 3 invited (last changed ")
        );
        assert_eq!(
            line(&lines, "Encryption:"),
            "Encryption: m.megolm.v1.aes-sha2; key rotation every 7d or 100 messages"
        );
        assert_eq!(
            line(&lines, "Join rule:"),
            "Join rule: restricted (members of !space:example.com; org.example.custom)"
        );
        assert_eq!(
            line(&lines, "History visibility:"),
            "History visibility: shared"
        );
        assert_eq!(line(&lines, "Guest access:"), "Guest access: forbidden");
        assert_eq!(line(&lines, "Room type:"), "Room type: m.space");
        assert_eq!(line(&lines, "Room version:"), "Room version: 11");
        // The content omits `creator`, so the create event's sender stands in.
        assert_eq!(line(&lines, "Creator:"), "Creator: @founder:example.com");
        assert!(line(&lines, "Created:").starts_with("Created: "));
        assert_eq!(line(&lines, "Federation:"), "Federation: disabled");
        assert_eq!(
            line(&lines, "Upgraded from:"),
            "Upgraded from: !old:example.com"
        );
        assert_eq!(
            line(&lines, "Replaced by:"),
            "Replaced by: !new:example.com"
        );
        assert_eq!(
            line(&lines, "Server ACL:"),
            "Server ACL: allow 1, deny 2, IP literals denied"
        );
        assert_eq!(
            line(&lines, "Power levels"),
            "Power levels (configured values, not resolved permissions):"
        );
        assert!(lines.contains(&"  ban 50, kick 50, invite 0, redact unset".to_owned()));
        assert!(lines.contains(&"  notifications.room 50".to_owned()));
        assert!(lines.contains(&"    @admin:example.com  100".to_owned()));
        assert!(lines.contains(&"    m.room.name  50".to_owned()));
        assert!(!lines.iter().any(|line| line.contains("Room version 12+")));
    }

    #[test]
    fn unknown_snapshots_never_read_as_unset() {
        let state = loaded(
            Some(all_unknown()),
            Some(RoomInfoDto {
                member_counts: Some(None),
            }),
            Some(RoomUpgradeDto::default()),
        );
        let lines = lines(&state);
        let text = lines.join("\n");

        // The summary alias still shows; the advertised list is unknown, not empty.
        assert_eq!(
            line(&lines, "Canonical alias:"),
            "Canonical alias: #summary:example.com"
        );
        assert_eq!(
            line(&lines, "Advertised aliases:"),
            "Advertised aliases: unknown (not cached)"
        );
        assert_eq!(
            line(&lines, "Encryption:"),
            "Encryption: no encryption state cached"
        );
        assert!(!text.to_lowercase().contains("unencrypted"));
        assert_eq!(
            line(&lines, "Members:"),
            "Members: unknown (not yet observed)"
        );
        assert!(!text.contains("0 joined"));
        assert_eq!(
            line(&lines, "Join rule:"),
            "Join rule: unknown (not cached)"
        );
        assert_eq!(
            line(&lines, "Guest access:"),
            "Guest access: unknown (not cached)"
        );
        assert_eq!(
            line(&lines, "Room version:"),
            "Room version: unknown (not cached)"
        );
        assert_eq!(
            line(&lines, "Upgraded from:"),
            "Upgraded from: unknown (not cached)"
        );
        assert_eq!(line(&lines, "Replaced by:"), "Replaced by: none known");
        assert_eq!(
            line(&lines, "Power levels"),
            "Power levels (configured values, not resolved permissions): unknown (not cached)"
        );
        // Usually unknown, so it stays out of the popup until a tuple exists.
        assert!(!text.contains("Server ACL"));
    }

    #[test]
    fn contentless_statuses_have_distinct_wording() {
        for (status, expected) in [
            ("unavailable", "unavailable (content not retained)"),
            ("invalid", "unavailable (malformed state)"),
            ("too_large", "unavailable (state too large)"),
            (
                "a_status_from_the_future",
                "unavailable (unrecognized status)",
            ),
        ] {
            let mut metadata = all_unknown();
            metadata.join_rules =
                serde_json::from_value(snapshot(status, serde_json::Value::Null)).unwrap();
            metadata.server_acl =
                serde_json::from_value(snapshot(status, serde_json::Value::Null)).unwrap();
            let lines = lines(&loaded(Some(metadata), None, None));

            assert_eq!(line(&lines, "Join rule:"), format!("Join rule: {expected}"));
            assert_eq!(
                line(&lines, "Server ACL:"),
                format!("Server ACL: {expected}")
            );
        }
    }

    #[test]
    fn partial_snapshot_marks_withheld_fields_without_defaults() {
        let mut metadata = full_metadata();
        metadata.power_levels = serde_json::from_value(json!({
            "status": "partial",
            "invalid_fields": ["ban", "users.*"],
            "content": { "kick": 50, "users": { "@admin:example.com": 100 } },
        }))
        .unwrap();
        metadata.aliases = serde_json::from_value(json!({
            "status": "partial",
            "invalid_fields": ["alt_aliases[]"],
            "content": { "alias": "#ops:example.com", "alt_aliases": ["#a:example.com"] },
        }))
        .unwrap();
        metadata.join_rules = serde_json::from_value(json!({
            "status": "partial",
            "invalid_fields": ["join_rule", "allow[]"],
            "content": { "allow": [] },
        }))
        .unwrap();
        let lines = lines(&loaded(Some(metadata), None, None));

        assert!(lines.contains(
            &"  ban withheld (malformed), kick 50, invite unset, redact unset".to_owned()
        ));
        assert!(lines.contains(&"  Users: (some entries withheld)".to_owned()));
        assert_eq!(
            line(&lines, "Advertised aliases:"),
            "Advertised aliases: #a:example.com (some entries withheld)"
        );
        assert_eq!(
            line(&lines, "Join rule:"),
            "Join rule: withheld (malformed) (some conditions withheld)"
        );
    }

    #[test]
    fn redaction_evidence_is_shown_with_and_without_content() {
        let mut metadata = full_metadata();
        metadata.join_rules.redacted = Some(true);
        metadata.history_visibility = MetadataSnapshot {
            status: SnapshotStatus::Unavailable,
            redacted: Some(true),
            ..MetadataSnapshot::default()
        };
        let lines = lines(&loaded(Some(metadata), None, None));

        assert_eq!(
            line(&lines, "Join rule:"),
            "Join rule: restricted (members of !space:example.com; org.example.custom) \
             (redacted; retained fields shown)"
        );
        assert_eq!(
            line(&lines, "History visibility:"),
            "History visibility: redacted"
        );
    }

    #[test]
    fn v12_room_with_no_configured_users_explains_creator_privilege() {
        let mut metadata = full_metadata();
        metadata.creation.content = Some(RoomCreationContent {
            room_version: Some("12".to_owned()),
            additional_creators: Some(vec!["@cofounder:example.com".to_owned()]),
            predecessor: Some(RoomPredecessorContent {
                room_id: "!old:example.com".to_owned(),
            }),
            ..RoomCreationContent::default()
        });
        metadata.power_levels.content = Some(RoomPowerLevelsContent {
            users: Some(Default::default()),
            ..RoomPowerLevelsContent::default()
        });
        let lines = lines(&loaded(Some(metadata), None, None));

        assert_eq!(
            line(&lines, "Creator:"),
            "Creator: @founder:example.com; additional creators: @cofounder:example.com"
        );
        assert!(lines.contains(&"  Users: none configured".to_owned()));
        assert!(lines.iter().any(|line| line.contains("Room version 12+")));
    }

    #[test]
    fn pending_reads_say_so_instead_of_guessing() {
        let mut state = loaded(None, None, None);
        state.in_flight = ROOM_INFO_READS;
        let loading = lines(&state);
        assert_eq!(line(&loading, "Members:"), "Members: loading…");
        assert_eq!(
            line(&loading, "Encryption, access"),
            "Encryption, access, and room details: loading…"
        );
        assert_eq!(
            line(&loading, "Canonical alias:"),
            "Canonical alias: #summary:example.com"
        );

        let never = detail_lines(&room(), None, TimeFormat::H24);
        assert_eq!(line(&never, "Members:"), "Members: not loaded");
    }

    #[test]
    fn state_for_another_room_is_not_rendered() {
        let mut state = loaded(Some(full_metadata()), None, None);
        state.key.room_id = "!other:example.com".to_owned();

        let lines = lines(&state);

        assert_eq!(
            line(&lines, "Advertised aliases:"),
            "Advertised aliases: not loaded"
        );
    }

    #[test]
    fn server_shapes_decode_leniently() {
        // A server that predates `member_counts` omits the field entirely.
        let info: RoomInfoDto =
            serde_json::from_value(json!({ "join_rule": "public", "encryption_algorithm": null }))
                .unwrap();
        assert_eq!(info.member_counts, None);
        assert_eq!(
            member_count_line(&View::Loaded(&info), TimeFormat::H24),
            "Members: unavailable (this axon server does not report member counts)"
        );
        let info: RoomInfoDto = serde_json::from_value(json!({
            "member_counts": { "joined": 2, "invited": 0, "observed_at": 5 },
        }))
        .unwrap();
        assert_eq!(
            info.member_counts.flatten().map(|counts| counts.joined),
            Some(2)
        );

        // A snapshot the server does not send reads as unknown.
        let metadata: RoomMetadataDto = serde_json::from_value(json!({})).unwrap();
        assert_eq!(metadata, all_unknown());
    }

    #[tokio::test]
    async fn whereami_fetches_once_and_keeps_failures_explicit() {
        let mut app = app();
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.room_info.tx = Some(tx);

        app.handle_command(crate::command::Command::Whereami).await;
        let generation = app.room_info.generation;
        // A second request while the first is in flight must not start another.
        app.request_room_info(key());
        assert_eq!(app.room_info.generation, generation);

        for _ in 0..ROOM_INFO_READS {
            let outcome = rx.recv().await.expect("each read reports back");
            app.apply_room_info_outcome(outcome);
        }
        assert!(
            rx.try_recv().is_err(),
            "the duplicate request spawned reads"
        );

        let text = crate::ui::popup_room_info_lines(&app).join("\n");
        assert!(
            text.contains("Members: unavailable (request failed)"),
            "{text}"
        );
        assert!(!text.contains("API support needed"), "{text}");
    }

    /// Serve `connections` requests, answering by the path's last segment,
    /// and return the request targets seen.
    async fn stub_axon(
        connections: usize,
        respond: fn(&str) -> String,
    ) -> (AxonClient, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = AxonClient::new(format!("http://{}", listener.local_addr().unwrap()), None);
        let server = tokio::spawn(async move {
            let mut targets = Vec::new();
            for _ in 0..connections {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    let mut chunk = [0; 1024];
                    let read = socket.read(&mut chunk).await.unwrap();
                    assert!(read > 0);
                    bytes.extend_from_slice(&chunk[..read]);
                }
                let request = String::from_utf8(bytes).unwrap();
                let target = request.split_whitespace().nth(1).unwrap().to_owned();
                let body = respond(target.rsplit('/').next().unwrap());
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(head.as_bytes()).await.unwrap();
                // The client may hang up on an oversized body; that is the point.
                let _ = socket.write_all(body.as_bytes()).await;
                targets.push(target);
            }
            targets
        });
        (client, server)
    }

    #[tokio::test]
    async fn whereami_reads_all_four_endpoints_over_http() {
        let (client, server) = stub_axon(usize::from(ROOM_INFO_READS), |endpoint| {
            match endpoint {
                "metadata" => json!({ "data": {
                    "encryption": snapshot("available", json!({ "algorithm": "m.megolm.v1.aes-sha2" })),
                    "creation": snapshot("available", json!({ "room_version": "11" })),
                }}),
                "info" => json!({ "data": {
                    "member_counts": { "joined": 412, "invited": 3, "observed_at": 1_700_000_000_000_i64 },
                    "join_rule": "public", "history_visibility": null,
                    "guest_access": null, "encryption_algorithm": null,
                }}),
                "upgrade" => json!({ "data": { "tombstoned_to": null, "upgraded_from": null } }),
                "members" => json!({ "data": [{
                    "user_id": "@ada:example.com",
                    "membership": "join",
                    "display_name": "Ada",
                }]}),
                other => panic!("unexpected endpoint {other}"),
            }
            .to_string()
        })
        .await;
        let mut app = app();
        app.client = client;
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.room_info.tx = Some(tx);

        app.handle_command(crate::command::Command::Whereami).await;
        for _ in 0..ROOM_INFO_READS {
            let outcome = rx.recv().await.expect("each read reports back");
            app.apply_room_info_outcome(outcome);
        }

        let mut targets = server.await.unwrap();
        targets.sort();
        let base = format!("/v1/accounts/{}/rooms/%21room%3Aexample.com", Uuid::nil());
        assert_eq!(
            targets,
            ["info", "members", "metadata", "upgrade"].map(|endpoint| format!("{base}/{endpoint}"))
        );
        let lines = crate::ui::popup_room_info_lines(&app);
        assert_eq!(
            line(&lines, "Encryption:"),
            "Encryption: m.megolm.v1.aes-sha2"
        );
        assert_eq!(line(&lines, "Room version:"), "Room version: 11");
        assert!(line(&lines, "Members:").starts_with("Members: 412 joined, 3 invited"));
        assert_eq!(
            line(&lines, "Members ("),
            "Members (1 loaded of 412 joined)"
        );
        assert!(lines.iter().any(|line| line.contains("@ada:example.com")));
        assert_eq!(line(&lines, "Replaced by:"), "Replaced by: none known");
        // Snapshots the stub left out are unknown, not unset.
        assert_eq!(
            line(&lines, "Join rule:"),
            "Join rule: unknown (not cached)"
        );
    }

    #[tokio::test]
    async fn oversized_room_reads_are_refused() {
        let (client, _server) = stub_axon(1, |_| {
            format!(r#"{{"data":{{"padding":"{}"}}}}"#, "x".repeat(128 * 1024))
        })
        .await;

        let result = client.room_info(Uuid::nil(), "!room:example.com").await;

        assert!(
            matches!(&result, Err(err) if err.to_string().contains("size limit")),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn oversized_member_list_is_refused() {
        let (client, _server) = stub_axon(1, |_| {
            format!(
                r#"{{"data":[{{"user_id":"@a:example.com","membership":"join","display_name":"{}"}}]}}"#,
                "x".repeat(crate::api::ROOM_MEMBERS_LIMIT)
            )
        })
        .await;

        let result = client
            .room_members_bounded(Uuid::nil(), "!room:example.com")
            .await;

        assert!(
            matches!(&result, Err(err) if err.to_string().contains("size limit")),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn sender_name_member_read_accepts_a_body_over_the_whereami_cap() {
        let (client, _server) = stub_axon(1, |_| {
            format!(
                r#"{{"data":[{{"user_id":"@a:example.com","membership":"join","display_name":"{}"}}]}}"#,
                "x".repeat(crate::api::ROOM_MEMBERS_LIMIT)
            )
        })
        .await;

        let result = client.room_members(Uuid::nil(), "!room:example.com").await;

        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn labels_and_gaps_are_coloured_without_changing_the_text() {
        let colors = TuiConfig::test_default().colors;
        let label = Style::default().fg(colors.selected_room);
        let gap = Style::default().fg(colors.input_hint);
        let spans = |text: &str| {
            let line = styled_line(text.to_owned(), &colors);
            assert_eq!(line.to_string(), text, "styling must not alter the text");
            line.spans
                .into_iter()
                .map(|span| (span.content.into_owned(), span.style))
                .collect::<Vec<_>>()
        };

        // Only the first separator splits: the value keeps its own colons.
        assert_eq!(
            spans("Matrix ID: !room:example.com"),
            [
                ("Matrix ID: ".to_owned(), label),
                ("!room:example.com".to_owned(), Style::default())
            ]
        );
        assert_eq!(
            spans("Guest access: unknown (not cached)"),
            [
                ("Guest access: ".to_owned(), label),
                ("unknown (not cached)".to_owned(), gap)
            ]
        );
        assert_eq!(
            spans("Power levels (configured values, not resolved permissions):"),
            [(
                "Power levels (configured values, not resolved permissions):".to_owned(),
                label
            )]
        );
        // List entries and prose stay plain, colon or not.
        for plain in [
            "  Re: lunch  @alice:example.com  (join)",
            "    m.room.name  50",
            "Latest refresh failed; some details above may be out of date.",
        ] {
            assert_eq!(spans(plain), [(plain.to_owned(), Style::default())]);
        }
    }

    #[test]
    fn wrapping_keeps_words_styles_and_every_character() {
        let label = Style::default().fg(ratatui::style::Color::Cyan);
        let line = Line::from(vec![
            Span::styled("Advertised aliases: ", label),
            Span::raw("#one:example.com, #two:example.com, #three:example.com"),
        ]);
        let text = line.to_string();

        let rows = wrap_styled_line(line, 30);

        assert!(rows.len() > 1);
        assert!(rows.iter().all(|row| row.width() <= 30), "{rows:#?}");
        // Nothing is dropped or reordered, and no alias is split.
        let words = |text: &str| {
            text.split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let joined = rows
            .iter()
            .map(|row| row.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(words(&joined), words(&text));
        assert!(rows
            .iter()
            .any(|row| row.to_string().trim() == "#two:example.com,"));
        // The label keeps its colour; continuation rows hang two columns in,
        // unstyled, and never start with the space that was broken at.
        assert_eq!(rows[0].spans[0].style, label);
        for row in &rows[1..] {
            let text = row.to_string();
            assert!(
                text.starts_with("  ") && !text.starts_with("   "),
                "{text:?}"
            );
            assert_eq!(row.spans[0].style, Style::default());
        }

        // A word wider than the row is broken rather than overflowing.
        let rows = wrap_styled_line(Line::from("x".repeat(25)), 10);
        assert_eq!(
            rows.iter().map(|row| row.width()).collect::<Vec<_>>(),
            [10, 10, 9]
        );
        assert_eq!(wrap_styled_line(Line::from(""), 10).len(), 1);
        // Too narrow to hang an indent, or to hold one wide character: still
        // terminates, one character per row.
        assert_eq!(wrap_styled_line(Line::from("ab"), 1).len(), 2);
        assert_eq!(wrap_styled_line(Line::from("日本"), 1).len(), 2);
    }

    #[test]
    fn wrapping_an_indented_entry_never_emits_a_blank_row() {
        // The only spaces before the overflow are the entry's own indent.
        let entry = format!("    @{}:example.com  100", "u".repeat(40));

        let rows: Vec<String> = wrap_styled_line(Line::from(entry.clone()), 30)
            .iter()
            .map(|row| row.to_string())
            .collect();

        assert!(rows.iter().all(|row| !row.trim().is_empty()), "{rows:#?}");
        assert!(rows
            .iter()
            .all(|row| Line::from(row.as_str()).width() <= 30));
        assert!(rows[0].starts_with("    @uuu"));
        // Continuations sit two columns inside the entry's indent.
        assert!(rows[1..]
            .iter()
            .all(|row| row.starts_with("      ") && !row.starts_with("       ")));
        let unwrapped: String = rows.concat().replace(' ', "");
        assert_eq!(unwrapped, entry.replace(' ', ""));

        // Too narrow to hang an indent: the indent's own spaces must still
        // not be taken as the place to break.
        let narrow: Vec<String> = wrap_styled_line(Line::from("    abcdef"), 6)
            .iter()
            .map(|row| row.to_string())
            .collect();
        assert_eq!(narrow, ["    ab", "cdef"]);
    }

    #[test]
    fn room_supplied_text_is_neither_dimmed_as_a_gap_nor_sent_raw() {
        let colors = TuiConfig::test_default().colors;
        let gap = Style::default().fg(colors.input_hint);

        // A topic that happens to start like a gap phrase is still a topic.
        let topic = styled_line("Topic: unavailable on weekends".to_owned(), &colors);
        assert_eq!(topic.spans[1].style, Style::default());
        let detail = styled_line(
            "Guest access: unavailable (malformed state)".to_owned(),
            &colors,
        );
        assert_eq!(detail.spans[1].style, gap);

        // Escape sequences and line breaks from room state never reach the
        // terminal, and cannot add rows the pager did not count.
        let hostile = styled_line(
            "Advertised aliases: #a\u{1b}[2J:x\nName: spoofed".to_owned(),
            &colors,
        );
        let text = hostile.to_string();
        assert!(!text.contains(char::is_control), "{text:?}");
        assert_eq!(text, "Advertised aliases: #a\u{fffd}[2J:x Name: spoofed");
    }

    #[test]
    fn a_loaded_upgrade_read_never_leaves_an_empty_value() {
        // `/upgrade` answered; `/metadata` has not, or failed.
        for failure in [None, Some(ReadFailure::Failed)] {
            let mut state = loaded(None, None, Some(RoomUpgradeDto::default()));
            state.in_flight = 1;
            state.metadata.failure = failure;
            let lines = lines(&state);

            assert_eq!(line(&lines, "Upgraded from:"), "Upgraded from: none known");
            assert!(lines.iter().all(|line| !line.ends_with(": ")), "{lines:#?}");
        }
    }

    #[tokio::test]
    async fn a_retry_reads_as_loading_until_it_settles() {
        let mut app = app();
        let (tx, _rx) = mpsc::unbounded_channel();
        app.room_info.tx = Some(tx);
        let mut state = loaded(Some(full_metadata()), None, None);
        state.metadata.failure = Some(ReadFailure::Failed);
        state.info.failure = Some(ReadFailure::Failed);
        app.room_info.state = Some(state);
        let before = crate::ui::popup_room_info_lines(&app);
        assert_eq!(
            line(&before, "Members:"),
            "Members: unavailable (request failed)"
        );
        assert!(before
            .iter()
            .any(|line| line.starts_with("Latest refresh failed")));

        app.request_room_info(key());

        let during = crate::ui::popup_room_info_lines(&app);
        assert_eq!(line(&during, "Members:"), "Members: loading…");
        assert!(!during
            .iter()
            .any(|line| line.starts_with("Latest refresh failed")));
        // The data it already had stays up while the retry runs.
        assert_eq!(line(&during, "Room version:"), "Room version: 11");
    }

    #[tokio::test]
    async fn an_older_server_without_the_endpoint_is_not_a_failed_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = AxonClient::new(format!("http://{}", listener.local_addr().unwrap()), None);
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut chunk = [0; 2048];
                let _ = socket.read(&mut chunk).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await;
            }
        });
        let mut app = app();
        app.client = client;
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.room_info.tx = Some(tx);

        app.request_room_info(key());
        for _ in 0..ROOM_INFO_READS {
            let outcome = rx.recv().await.expect("each read reports back");
            app.apply_room_info_outcome(outcome);
        }

        let lines = crate::ui::popup_room_info_lines(&app);
        assert_eq!(
            line(&lines, "Members:"),
            "Members: unavailable (this axon server does not provide it)"
        );
        assert_eq!(
            line(&lines, "Encryption, access"),
            "Encryption, access, and room details: unavailable (this axon server does not provide it)"
        );
    }

    #[test]
    fn a_field_taller_than_the_popup_can_be_scrolled_to_its_end() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut aliases: Vec<String> = (0..200).map(|n| format!("#a{n}:example.com")).collect();
        aliases.push("#FINALALIAS:example.com".to_owned());
        let mut metadata = all_unknown();
        metadata.aliases = serde_json::from_value(snapshot(
            "available",
            json!({ "alias": "#ops:example.com", "alt_aliases": aliases }),
        ))
        .unwrap();
        let mut app = app();
        app.room_info.state = Some(loaded(Some(metadata), None, None));
        app.mode = crate::app::Mode::Popup(crate::app::PopupKind::RoomInfo);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

        let mut seen_at = None;
        let mut rows = 0;
        for scroll in 0..2000 {
            app.popup_scroll = scroll;
            let frame = terminal
                .draw(|frame| {
                    crate::ui::prepare(&mut app, frame.area());
                    crate::ui::draw(frame, &mut app);
                })
                .unwrap();
            let screen: String = frame
                .buffer
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            if screen.contains("FINALALIAS") {
                seen_at.get_or_insert(scroll);
            }
            rows = app.frame.popup.as_ref().unwrap().lines.len();
            if app.popup_scroll < scroll {
                break; // clamped: every offset has been drawn
            }
        }

        assert!(
            seen_at.is_some(),
            "the final advertised alias never appears at any popup scroll offset"
        );
        // Paged row by row: the list spans several 22-row pages.
        assert!(
            rows > 60,
            "expected the alias list as many rows, got {rows}"
        );
    }

    /// Hold every connection open and report how many are open at once.
    async fn stalled_axon() -> (
        AxonClient,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = AxonClient::new(format!("http://{}", listener.local_addr().unwrap()), None);
        let (open, accepted) = (
            std::sync::Arc::new(AtomicUsize::new(0)),
            std::sync::Arc::new(AtomicUsize::new(0)),
        );
        let counters = (open.clone(), accepted.clone());
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (open, accepted) = counters.clone();
                open.fetch_add(1, Ordering::SeqCst);
                accepted.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    // Never answer; a read of 0 (or an error) is the client hanging up.
                    let mut chunk = [0; 1024];
                    while matches!(socket.read(&mut chunk).await, Ok(read) if read > 0) {}
                    open.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        (client, open, accepted)
    }

    #[tokio::test]
    async fn changing_rooms_does_not_accumulate_pending_reads() {
        use std::sync::atomic::Ordering;
        let (client, open, accepted) = stalled_axon().await;
        let mut app = app();
        app.client = client;
        let (tx, _rx) = mpsc::unbounded_channel();
        app.room_info.tx = Some(tx);
        let reads = usize::from(ROOM_INFO_READS);
        let mut other = key();
        other.room_id = "!other:example.com".to_owned();
        let settle = |target: usize, counter: std::sync::Arc<std::sync::atomic::AtomicUsize>| async move {
            for _ in 0..500 {
                if counter.load(Ordering::SeqCst) == target {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        };

        // A, B, A while axon answers nothing: each batch must reach the server
        // before the next replaces it, or the abort would be untested.
        for (round, room) in [key(), other, key()].into_iter().enumerate() {
            app.request_room_info(room);
            settle((round + 1) * reads, accepted.clone()).await;
            assert_eq!(accepted.load(Ordering::SeqCst), (round + 1) * reads);
        }
        settle(reads, open.clone()).await;

        assert_eq!(
            open.load(Ordering::SeqCst),
            reads,
            "one fetch should bound live reads to {reads}"
        );
    }

    #[test]
    fn stale_and_superseded_results_are_dropped() {
        let mut app = app();
        let mut state = loaded(None, None, None);
        state.generation = 2;
        state.in_flight = ROOM_INFO_READS;
        app.room_info.state = Some(state);
        let info = || RoomInfoPart::Info(Ok(RoomInfoDto::default()));

        let mut other = key();
        other.room_id = "!other:example.com".to_owned();
        app.apply_room_info_outcome(RoomInfoOutcome {
            key: other.clone(),
            generation: 2,
            part: info(),
        });
        app.apply_room_info_outcome(RoomInfoOutcome {
            key: other,
            generation: 2,
            part: RoomInfoPart::Members(Ok(vec![listed_member(
                "join",
                "@other:example.com",
                Some("Other"),
            )])),
        });
        app.apply_room_info_outcome(RoomInfoOutcome {
            key: key(),
            generation: 1,
            part: info(),
        });
        let state = app.room_info.state.as_ref().unwrap();
        assert!(state.info.value.is_none());
        assert!(state.members.value.is_none());
        assert_eq!(state.in_flight, ROOM_INFO_READS);

        app.apply_room_info_outcome(RoomInfoOutcome {
            key: key(),
            generation: 2,
            part: info(),
        });
        assert!(app.room_info.state.as_ref().unwrap().info.value.is_some());
    }

    #[test]
    fn failed_refresh_keeps_last_good_data_and_flags_it() {
        let mut app = app();
        app.room_info.state = Some(loaded(Some(full_metadata()), None, None));

        app.apply_room_info_outcome(RoomInfoOutcome {
            key: key(),
            generation: 0,
            part: RoomInfoPart::Metadata(Err(ReadFailure::Failed)),
        });

        let lines = crate::ui::popup_room_info_lines(&app);
        assert_eq!(line(&lines, "Room version:"), "Room version: 11");
        assert!(lines
            .iter()
            .any(|line| line.starts_with("Latest refresh failed")));
    }

    fn listed_member(membership: &str, user_id: &str, name: Option<&str>) -> MemberDto {
        MemberDto {
            user_id: user_id.to_owned(),
            membership: membership.to_owned(),
            display_name: name.map(str::to_owned),
        }
    }

    fn room_info_with(members: Vec<MemberDto>, info: Option<RoomInfoDto>) -> RoomInfoState {
        let mut state = with_members(members, info);
        state.metadata.value = Some(full_metadata());
        store_member_lines(&mut state);
        state
    }

    fn with_members(members: Vec<MemberDto>, info: Option<RoomInfoDto>) -> RoomInfoState {
        let mut state = loaded(None, info, None);
        state.members.value = Some(members);
        store_member_lines(&mut state);
        state
    }

    fn counts(joined: i64) -> RoomInfoDto {
        RoomInfoDto {
            member_counts: Some(Some(RoomMemberCountsDto {
                joined,
                invited: 0,
                observed_at: 5,
            })),
        }
    }

    #[test]
    fn member_list_says_when_the_joined_count_exceeds_it() {
        let state = with_members(
            vec![
                listed_member("join", "@ada:example.com", Some("Ada")),
                listed_member("leave", "@old:example.com", Some("Old")),
            ],
            Some(counts(412)),
        );

        let lines = member_list_lines(&room(), Some(&state));

        assert_eq!(lines[0], "Members (2 loaded of 412 joined)");
        assert!(lines.iter().any(|line| line.contains("@ada:example.com")));
        assert!(lines.iter().any(|line| line.contains("@old:example.com")));
        assert!(!lines.iter().any(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("complete") || lower.contains("all members")
        }));
    }

    #[test]
    fn member_list_does_not_invent_a_joined_count() {
        let members = vec![listed_member("join", "@ada:example.com", Some("Ada"))];
        let absent = RoomInfoDto {
            member_counts: None,
        };
        let unknown = RoomInfoDto {
            member_counts: Some(None),
        };
        for info in [Some(absent), Some(unknown), None] {
            let lines = member_list_lines(&room(), Some(&with_members(members.clone(), info)));
            assert_eq!(lines[0], "Members (1 loaded; joined count unknown)");
            assert!(!lines[0].contains(" of "));
        }

        let equal = with_members(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
        );
        let lines = member_list_lines(&room(), Some(&equal));
        assert_eq!(lines[0], "Members (1 loaded)");

        let empty = with_members(Vec::new(), Some(counts(0)));
        assert_eq!(
            member_list_lines(&room(), Some(&empty)),
            vec!["Members (0 loaded)".to_owned(), "  none loaded".to_owned()]
        );
    }

    #[test]
    fn member_list_groups_and_orders_every_loaded_row() {
        let state = with_members(
            vec![
                listed_member("leave", "@dan:example.com", Some("Dan")),
                listed_member("knock", "@erin:example.com", Some("Erin")),
                listed_member("join", "@zoe:example.com", Some("Zoe")),
                listed_member("join", "@amz:example.com", Some("amy")),
                listed_member("join", "@amy:example.com", Some("Amy")),
                listed_member("join", "@aaron:example.com", Some("   ")),
                listed_member("ban", "@cara:example.com", Some("Cara")),
                listed_member("invite", "@bob:example.com", Some("Bob")),
                listed_member("", "@fay:example.com", Some("Fay")),
            ],
            Some(counts(9)),
        );

        assert_eq!(
            member_list_lines(&room(), Some(&state)),
            vec![
                "Members (9 loaded)".to_owned(),
                "  Joined".to_owned(),
                "    unknown  @aaron:example.com  configured power unknown".to_owned(),
                "    Amy  @amy:example.com  configured power unknown".to_owned(),
                "    amy  @amz:example.com  configured power unknown".to_owned(),
                "    Zoe  @zoe:example.com  configured power unknown".to_owned(),
                "  Invited".to_owned(),
                "    Bob  @bob:example.com  configured power unknown".to_owned(),
                "  Banned".to_owned(),
                "    Cara  @cara:example.com  configured power unknown".to_owned(),
                "  Left".to_owned(),
                "    Dan  @dan:example.com  configured power unknown".to_owned(),
                "  (unset membership)".to_owned(),
                "    Fay  @fay:example.com  configured power unknown".to_owned(),
                "  knock".to_owned(),
                "    Erin  @erin:example.com  configured power unknown".to_owned(),
            ]
        );
    }

    #[test]
    fn member_power_is_the_configured_users_entry_only() {
        let mut state = with_members(
            vec![
                listed_member("join", "@admin:example.com", Some("Admin")),
                listed_member("join", "@plain:example.com", Some("Plain")),
            ],
            Some(counts(2)),
        );
        state.metadata.value = Some(full_metadata());

        let lines = member_list_lines(&room(), Some(&state));
        assert!(lines.contains(&"    Admin  @admin:example.com  configured power 100".to_owned()));
        assert!(
            lines.contains(&"    Plain  @plain:example.com  no per-user power level".to_owned())
        );
        assert!(!lines.iter().any(|line| line.contains("configured power 0")));

        state.metadata.value.as_mut().unwrap().power_levels = serde_json::from_value(json!({
            "status": "partial",
            "invalid_fields": ["users.*"],
            "content": { "users": { "@admin:example.com": 100 }, "users_default": 0 },
        }))
        .unwrap();
        let withheld = member_list_lines(&room(), Some(&state));
        assert!(withheld
            .iter()
            .filter(
                |line| line.contains("@admin:example.com") || line.contains("@plain:example.com")
            )
            .all(|line| line.contains("configured power unknown")));

        state.metadata.value = Some(all_unknown());
        let unknown = member_list_lines(&room(), Some(&state));
        assert!(unknown
            .iter()
            .any(|line| line.contains("@admin:example.com  configured power unknown")));
    }

    #[test]
    fn v12_creators_are_not_given_a_numeric_power_level() {
        let mut metadata = full_metadata();
        metadata.creation.content = Some(RoomCreationContent {
            room_version: Some("12".to_owned()),
            additional_creators: Some(vec!["@cofounder:example.com".to_owned()]),
            ..RoomCreationContent::default()
        });
        metadata.creation.sender = Some("@founder:example.com".to_owned());
        metadata.power_levels.content.as_mut().unwrap().users = Some(
            [("@founder:example.com", 100), ("@admin:example.com", 50)]
                .into_iter()
                .map(|(id, level)| (id.to_owned(), level))
                .collect(),
        );
        let mut state = with_members(
            vec![
                listed_member("join", "@founder:example.com", Some("Founder")),
                listed_member("join", "@cofounder:example.com", Some("Cofounder")),
                listed_member("join", "@admin:example.com", Some("Admin")),
                listed_member("join", "@plain:example.com", Some("Plain")),
            ],
            Some(counts(4)),
        );
        state.metadata.value = Some(metadata);

        let lines = member_list_lines(&room(), Some(&state));
        assert!(lines.contains(
            &"    Founder  @founder:example.com  creator (power not represented)".to_owned()
        ));
        assert!(lines.contains(
            &"    Cofounder  @cofounder:example.com  creator (power not represented)".to_owned()
        ));
        assert!(lines.contains(&"    Admin  @admin:example.com  configured power 50".to_owned()));
        assert!(
            lines.contains(&"    Plain  @plain:example.com  no per-user power level".to_owned())
        );
        assert!(!lines.iter().any(|line| {
            line.contains("@founder:example.com") && line.contains("configured power")
        }));
    }

    #[test]
    fn member_list_ignores_timeline_membership_events() {
        let mut app = app();
        let mut state = with_members(
            vec![listed_member("join", "@bob:example.com", Some("Bob"))],
            Some(counts(1)),
        );
        state.metadata.value = Some(full_metadata());
        store_member_lines(&mut state);
        app.room_info.state = Some(state);
        app.messages.events.insert(
            key(),
            vec![crate::api::EventDto {
                account_id: Uuid::nil(),
                event_id: "$member:example.com".to_owned(),
                room_id: "!room:example.com".to_owned(),
                sender: "@alice:example.com".to_owned(),
                state_key: Some("@alice:example.com".to_owned()),
                arrival_order: 0,
                origin_ts: 0,
                event_type: "m.room.member".to_owned(),
                content: Some(json!({
                    "membership": "join",
                    "displayname": "Alice"
                })),
                body: None,
                relates_to: None,
                redacted: false,
                redaction_event_id: None,
                reactions: None,
                sender_trust: None,
            }],
        );

        let text = crate::ui::popup_room_info_lines(&app).join("\n");

        assert!(text.contains("Bob  @bob:example.com"));
        assert!(!text.contains("@alice:example.com"));
        assert!(!text.contains("loaded timeline"));
    }

    #[test]
    fn a_failed_member_read_does_not_fall_back_to_the_timeline() {
        let mut app = app();
        let mut state = loaded(None, Some(counts(4)), None);
        state.members.failure = Some(ReadFailure::Failed);
        store_member_lines(&mut state);
        app.room_info.state = Some(state);
        app.messages.events.insert(
            key(),
            vec![crate::api::EventDto {
                account_id: Uuid::nil(),
                event_id: "$member:example.com".to_owned(),
                room_id: "!room:example.com".to_owned(),
                sender: "@alice:example.com".to_owned(),
                state_key: Some("@alice:example.com".to_owned()),
                arrival_order: 0,
                origin_ts: 0,
                event_type: "m.room.member".to_owned(),
                content: Some(json!({ "membership": "join", "displayname": "Alice" })),
                body: None,
                relates_to: None,
                redacted: false,
                redaction_event_id: None,
                reactions: None,
                sender_trust: None,
            }],
        );

        let text = crate::ui::popup_room_info_lines(&app).join("\n");

        assert!(text.contains("Member list: unavailable (request failed)"));
        assert!(!text.contains("@alice:example.com"));
        assert!(!text.contains("loaded timeline"));
    }

    #[test]
    fn failed_member_refresh_keeps_the_loaded_list() {
        let mut app = app();
        let mut state = with_members(
            vec![listed_member("join", "@alice:example.com", Some("Alice"))],
            Some(counts(1)),
        );
        state.generation = 1;
        app.room_info.state = Some(state);

        app.apply_room_info_outcome(RoomInfoOutcome {
            key: key(),
            generation: 1,
            part: RoomInfoPart::Members(Err(ReadFailure::Failed)),
        });

        let lines = crate::ui::popup_room_info_lines(&app);
        assert!(lines
            .iter()
            .any(|line| line.contains("Alice  @alice:example.com")));
        assert!(lines
            .iter()
            .any(|line| line.starts_with("Latest refresh failed")));
    }

    #[test]
    fn a_missing_membership_decodes_as_unset() {
        let missing = serde_json::from_value::<MemberDto>(json!({
            "user_id": "@ada:example.com",
            "display_name": "Ada",
        }))
        .unwrap();
        assert_eq!(missing.membership, "");
        let null_membership = serde_json::from_value::<MemberDto>(json!({
            "user_id": "@ada:example.com",
            "membership": null,
        }));
        assert!(null_membership.is_err());
        let present = serde_json::from_value::<MemberDto>(json!({
            "user_id": "@ada:example.com",
            "membership": "join",
            "display_name": "Ada",
            "avatar_url": "mxc://example/ada",
        }))
        .unwrap();
        assert_eq!(present.membership, "join");
    }

    #[test]
    fn pruning_the_room_drops_its_information() {
        let mut app = app();
        app.room_info.state = Some(loaded(Some(full_metadata()), None, None));
        let mut other = key();
        other.room_id = "!other:example.com".to_owned();

        app.prune_room_info(&other);
        assert!(app.room_info.state.is_some());
        app.prune_room_info(&key());
        assert!(app.room_info.state.is_none());
    }

    fn filtered_lines(
        members: Vec<MemberDto>,
        info: Option<RoomInfoDto>,
        query: &str,
    ) -> Vec<String> {
        let mut state = room_info_with(members, info);
        state.member_filter = query.to_owned();
        store_member_lines(&mut state);
        member_list_lines(&room(), Some(&state))
    }

    #[test]
    fn member_filter_matches_display_name_or_matrix_id() {
        let members = vec![
            listed_member("join", "@ada:example.com", Some("Ada Lovelace")),
            listed_member("invite", "@bob:example.com", Some("Robert")),
        ];
        let by_name = filtered_lines(members.clone(), Some(counts(2)), "ada");
        assert!(by_name.iter().any(|line| line.contains("Ada Lovelace")));
        assert!(!by_name.iter().any(|line| line.contains("@bob")));
        assert!(by_name.contains(&"1 of 2 loaded members match".to_owned()));
        assert!(!by_name.iter().any(|line| line.contains("Invited")));

        let by_id = filtered_lines(members, Some(counts(2)), "bob:example");
        assert!(by_id.iter().any(|line| line.contains("@bob:example.com")));
        assert!(!by_id.iter().any(|line| line.contains("Ada Lovelace")));
    }

    #[test]
    fn member_filter_ignores_the_unknown_placeholder_and_power_text() {
        let mut state = room_info_with(
            vec![
                listed_member("join", "@aaron:example.com", Some("   ")),
                listed_member("join", "@admin:example.com", Some("Admin")),
                listed_member("join", "@ada:example.com", Some("Ärger")),
                listed_member("join", "@jose:example.com", Some("José")),
            ],
            Some(counts(4)),
        );

        state.member_filter = "unknown".to_owned();
        store_member_lines(&mut state);
        let unknown = member_list_lines(&room(), Some(&state));
        assert!(!unknown.iter().any(|line| line.contains("@aaron")));
        assert!(unknown
            .iter()
            .any(|line| line.contains("0 of 4 loaded members match")));

        state.member_filter = "aaron".to_owned();
        store_member_lines(&mut state);
        assert!(member_list_lines(&room(), Some(&state))
            .iter()
            .any(|line| line.contains("@aaron:example.com")));

        state.member_filter = "100".to_owned();
        store_member_lines(&mut state);
        let power = member_list_lines(&room(), Some(&state));
        assert!(!power.iter().any(|line| line.contains("@admin")));
        assert!(power.iter().any(|line| line.contains("none match")));

        state.member_filter = "ärger".to_owned();
        store_member_lines(&mut state);
        assert!(member_list_lines(&room(), Some(&state))
            .iter()
            .any(|line| line.contains("@ada:example.com")));

        state.member_filter = "jose".to_owned();
        store_member_lines(&mut state);
        let jose = member_list_lines(&room(), Some(&state));
        assert!(jose.iter().any(|line| line.contains("@jose:example.com")));
        assert!(!jose.iter().any(|line| line.contains("@ada")));

        state.member_filter = "strasse".to_owned();
        state.members.value = Some(vec![listed_member(
            "join",
            "@s:example.com",
            Some("Straße"),
        )]);
        store_member_lines(&mut state);
        assert!(member_list_lines(&room(), Some(&state))
            .iter()
            .any(|line| line.contains("@s:example.com")));
    }

    #[test]
    fn member_filter_with_no_matches_leaves_the_rest_of_the_popup() {
        let mut app = app();
        let mut state = room_info_with(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
        );
        state.member_filter = "zzz".to_owned();
        store_member_lines(&mut state);
        app.room_info.state = Some(state);

        let lines = crate::ui::popup_room_info_lines(&app);
        assert!(lines.iter().any(|line| line == "Name: Ops"));
        assert!(lines.iter().any(|line| line == "Room version: 11"));
        assert!(lines.contains(&"0 of 1 loaded members match".to_owned()));
        assert!(lines.contains(&"  none match".to_owned()));
        assert!(!lines
            .iter()
            .any(|line| line.contains("@ada:example.com") && line.starts_with("    ")));
    }

    #[test]
    fn member_filter_names_a_partial_roster_and_stays_quiet_without_a_joined_count() {
        let members = vec![
            listed_member("join", "@ada:example.com", Some("Ada")),
            listed_member("join", "@bob:example.com", Some("Bob")),
        ];
        let partial = filtered_lines(members.clone(), Some(counts(412)), "ada");
        assert_eq!(partial[0], "Members (2 loaded of 412 joined)");
        assert_eq!(
            partial[1],
            "1 of 2 loaded members match (room has more members than are loaded)"
        );

        let unknown = filtered_lines(members, None, "ada");
        assert_eq!(unknown[0], "Members (2 loaded; joined count unknown)");
        assert_eq!(unknown[1], "1 of 2 loaded members match");
        assert!(!unknown[1].contains("more members"));
    }

    #[test]
    fn a_blank_member_filter_shows_the_full_list() {
        let lines = filtered_lines(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
            "   ",
        );
        assert_eq!(lines[0], "Members (1 loaded)");
        assert!(lines.iter().any(|line| line.contains("@ada")));
        assert!(!lines
            .iter()
            .any(|line| line.contains("loaded members match")));
    }

    #[test]
    fn same_room_member_refresh_keeps_and_reapplies_the_filter() {
        let mut app = app();
        let mut state = room_info_with(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
        );
        state.generation = 1;
        state.member_filter = "ada".to_owned();
        state.member_filter_editing = true;
        store_member_lines(&mut state);
        app.room_info.state = Some(state);

        app.apply_room_info_outcome(RoomInfoOutcome {
            key: key(),
            generation: 1,
            part: RoomInfoPart::Members(Ok(vec![
                listed_member("join", "@ada:example.com", Some("Ada")),
                listed_member("join", "@zoe:example.com", Some("Zoe")),
            ])),
        });

        let state = app.room_info.state.as_ref().unwrap();
        assert_eq!(state.member_filter, "ada");
        assert!(state.member_filter_editing);
        let lines = member_list_lines(&room(), Some(state));
        assert!(lines.contains(&"1 of 2 loaded members match".to_owned()));
        assert!(lines.iter().any(|line| line.contains("@ada")));
        assert!(!lines.iter().any(|line| line.contains("@zoe")));
    }

    #[test]
    fn a_member_result_for_another_room_does_not_change_the_filter() {
        let mut app = app();
        let mut state = room_info_with(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
        );
        state.generation = 1;
        state.member_filter = "ada".to_owned();
        store_member_lines(&mut state);
        app.room_info.state = Some(state);
        let mut other = key();
        other.room_id = "!other:example.com".to_owned();

        app.apply_room_info_outcome(RoomInfoOutcome {
            key: other,
            generation: 1,
            part: RoomInfoPart::Members(Ok(vec![listed_member(
                "join",
                "@zoe:example.com",
                Some("Zoe"),
            )])),
        });

        let state = app.room_info.state.as_ref().unwrap();
        assert_eq!(state.member_filter, "ada");
        assert!(member_list_lines(&room(), Some(state))
            .iter()
            .any(|line| line.contains("@ada")));
        assert!(!member_list_lines(&room(), Some(state))
            .iter()
            .any(|line| line.contains("@zoe")));
    }

    #[tokio::test]
    async fn switching_the_cached_room_clears_the_member_filter() {
        let mut app = app();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.room_info.tx = Some(tx);
        let mut state = room_info_with(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
        );
        state.member_filter = "ada".to_owned();
        state.member_filter_editing = true;
        app.room_info.state = Some(state);
        let mut other = key();
        other.room_id = "!other:example.com".to_owned();

        app.request_room_info(other.clone());

        let fresh = app.room_info.state.as_ref().unwrap();
        assert_eq!(fresh.key, other);
        assert!(fresh.member_filter.is_empty());
        assert!(!fresh.member_filter_editing);
    }

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    #[tokio::test]
    async fn find_filters_members_and_esc_clears_before_it_closes() {
        let mut app = app();
        app.room_info.state = Some(room_info_with(
            vec![
                listed_member("join", "@ada:example.com", Some("Ada")),
                listed_member("join", "@bob:example.com", Some("Bob")),
            ],
            Some(counts(2)),
        ));
        app.mode = Mode::Popup(PopupKind::RoomInfo);
        app.popup_scroll = 4;

        app.handle_key(ctrl('f')).await;
        assert!(app.member_filter_is_editing());
        assert_eq!(app.popup_scroll, 4);

        for ch in ['a', 'd', 'a'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()))
                .await;
        }
        let lines = member_list_lines(&room(), app.room_info.state.as_ref());
        assert!(lines.iter().any(|line| line.contains("@ada")));
        assert!(!lines.iter().any(|line| line.contains("@bob")));
        crate::ui::prepare(&mut app, Rect::new(0, 0, 120, 30));
        let popup = app.frame.popup.as_ref().expect("room info popup");
        let heading = popup
            .lines
            .iter()
            .position(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    .starts_with("Members (")
            })
            .expect("member heading");
        assert!(
            (app.popup_scroll..app.popup_scroll + popup.page_size).contains(&heading),
            "typing should bring the member section on screen"
        );
        let at_members = app.popup_scroll;

        app.handle_key(KeyEvent::from(KeyCode::Down)).await;
        assert_eq!(app.popup_scroll, at_members + 1);
        assert!(app.member_filter_is_editing());
        app.handle_key(KeyEvent::from(KeyCode::PageDown)).await;
        assert_eq!(app.popup_scroll, at_members + 9);

        app.handle_key(KeyEvent::from(KeyCode::Enter)).await;
        assert!(!app.member_filter_is_editing());
        assert_eq!(app.mode, Mode::Popup(PopupKind::RoomInfo));
        assert_eq!(app.room_info.state.as_ref().unwrap().member_filter, "ada");
        app.handle_key(KeyEvent::from(KeyCode::Down)).await;
        assert_eq!(app.popup_scroll, at_members + 10);

        app.handle_key(KeyEvent::from(KeyCode::Esc)).await;
        assert_eq!(app.mode, Mode::Popup(PopupKind::RoomInfo));
        let lines = member_list_lines(&room(), app.room_info.state.as_ref());
        assert!(lines.iter().any(|line| line.contains("@bob")));
        assert!(!lines
            .iter()
            .any(|line| line.contains("loaded members match")));

        app.handle_key(KeyEvent::from(KeyCode::Esc)).await;
        assert_eq!(app.mode, Mode::Compose);
        assert!(app
            .room_info
            .state
            .as_ref()
            .unwrap()
            .member_filter
            .is_empty());
    }

    #[tokio::test]
    async fn an_empty_member_filter_prompt_closes_on_esc() {
        let mut app = app();
        app.room_info.state = Some(room_info_with(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
        ));
        app.mode = Mode::Popup(PopupKind::RoomInfo);

        app.handle_key(ctrl('f')).await;
        app.handle_key(KeyEvent::from(KeyCode::Esc)).await;

        assert_eq!(app.mode, Mode::Compose);
        assert!(!app.member_filter_is_editing());
    }

    #[tokio::test]
    async fn member_filter_follows_the_configured_find_binding() {
        let mut app = app();
        app.room_info.state = Some(room_info_with(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
        ));
        app.mode = Mode::Popup(PopupKind::RoomInfo);
        app.shortcuts.find = KeyBinding::from_spec("ctrl-g");

        app.handle_key(ctrl('f')).await;
        assert!(!app.member_filter_is_editing());

        app.handle_key(ctrl('g')).await;
        assert!(app.member_filter_is_editing());
    }

    #[tokio::test]
    async fn a_letter_bound_to_find_can_still_be_typed_into_the_filter() {
        let mut app = app();
        app.room_info.state = Some(room_info_with(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
        ));
        app.mode = Mode::Popup(PopupKind::RoomInfo);
        app.shortcuts.find = KeyBinding::from_spec("x");

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()))
            .await;
        assert!(app.member_filter_is_editing());
        assert!(app
            .room_info
            .state
            .as_ref()
            .unwrap()
            .member_filter
            .is_empty());

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()))
            .await;
        assert_eq!(app.room_info.state.as_ref().unwrap().member_filter, "x");
    }

    #[tokio::test]
    async fn paste_fills_an_open_member_filter_and_leaves_the_composer_alone() {
        let mut app = app();
        app.room_info.state = Some(room_info_with(
            vec![listed_member("join", "@bob:example.com", Some("Bob"))],
            Some(counts(1)),
        ));
        app.mode = Mode::Popup(PopupKind::RoomInfo);
        app.input.buffer = "keep".to_owned();

        app.handle_paste("Bob\n".to_owned());
        assert_eq!(app.input.buffer, "keep");
        assert!(app
            .room_info
            .state
            .as_ref()
            .unwrap()
            .member_filter
            .is_empty());

        app.handle_key(ctrl('f')).await;
        app.handle_paste("Bo\nb".to_owned());
        assert_eq!(app.room_info.state.as_ref().unwrap().member_filter, "Bob");
        assert_eq!(app.input.buffer, "keep");
        assert!(member_list_lines(&room(), app.room_info.state.as_ref())
            .iter()
            .any(|line| line.contains("@bob")));

        app.handle_paste("y".repeat(2_000));
        assert_eq!(
            app.room_info
                .state
                .as_ref()
                .unwrap()
                .member_filter
                .chars()
                .count(),
            MEMBER_FILTER_MAX_CHARS
        );
    }

    #[tokio::test]
    async fn the_shortcuts_popup_does_not_open_the_member_filter() {
        let mut app = app();
        app.room_info.state = Some(room_info_with(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
        ));
        app.mode = Mode::Popup(PopupKind::Shortcuts);

        app.handle_key(ctrl('f')).await;

        assert_eq!(app.mode, Mode::Popup(PopupKind::Shortcuts));
        assert!(!app.member_filter_is_editing());
    }

    #[tokio::test]
    async fn whitespace_around_a_member_filter_still_matches() {
        let mut app = app();
        app.room_info.state = Some(room_info_with(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
        ));
        app.mode = Mode::Popup(PopupKind::RoomInfo);
        app.handle_key(ctrl('f')).await;
        for ch in [' ', 'a', 'd', 'a'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()))
                .await;
        }
        assert!(member_list_lines(&room(), app.room_info.state.as_ref())
            .iter()
            .any(|line| line.contains("1 of 1 loaded members match")));

        app.handle_key(KeyEvent::from(KeyCode::Enter)).await;
        assert_eq!(app.room_info.state.as_ref().unwrap().member_filter, " ada");
        assert!(!app.member_filter_is_editing());
    }

    #[test]
    fn filtering_clamps_popup_scroll_and_leaves_a_fitting_offset() {
        let members: Vec<_> = (0..30)
            .map(|i| {
                listed_member(
                    "join",
                    &format!("@p{i}:example.com"),
                    Some(&format!("Person{i}")),
                )
            })
            .collect();
        let mut app = app();
        app.room_info.state = Some(room_info_with(members, Some(counts(30))));
        app.mode = Mode::Popup(PopupKind::RoomInfo);
        let area = Rect::new(0, 0, 120, 30);

        app.popup_scroll = 10_000;
        crate::ui::prepare(&mut app, area);
        let full_max = app.popup_scroll;
        assert!(
            full_max > 1,
            "the unfiltered popup has to be able to scroll"
        );

        app.popup_scroll = 10_000;
        app.room_info.state.as_mut().unwrap().member_filter = "Person0".to_owned();
        store_member_lines(app.room_info.state.as_mut().unwrap());
        crate::ui::prepare(&mut app, area);
        let filtered_max = app.popup_scroll;
        assert!(filtered_max < full_max);

        app.popup_scroll = 1;
        crate::ui::prepare(&mut app, area);
        assert_eq!(app.popup_scroll, 1);

        let title = app.frame.popup.as_ref().unwrap().title.clone();
        assert!(title.starts_with("Room Info  filter: Person0"));
    }

    #[test]
    fn a_long_member_filter_is_truncated_in_the_title_only() {
        let mut app = app();
        app.room_info.state = Some(room_info_with(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
        ));
        app.mode = Mode::Popup(PopupKind::RoomInfo);
        let query = "a".repeat(400);
        {
            let state = app.room_info.state.as_mut().unwrap();
            state.member_filter = query.clone();
            state.member_filter_editing = true;
            store_member_lines(state);
        }
        crate::ui::prepare(&mut app, Rect::new(0, 0, 120, 30));
        let title = &app.frame.popup.as_ref().unwrap().title;
        assert!(title.contains("..."));
        assert!(!title.contains(&query));
        assert_eq!(app.room_info.state.as_ref().unwrap().member_filter, query);
    }

    #[test]
    fn an_empty_roster_does_not_claim_zero_matches() {
        let mut state = room_info_with(Vec::new(), Some(counts(0)));
        state.member_filter = "ada".to_owned();
        store_member_lines(&mut state);
        let lines = member_list_lines(&room(), Some(&state));
        assert!(lines.iter().any(|line| line.contains("none loaded")));
        assert!(!lines
            .iter()
            .any(|line| line.contains("loaded members match")));
    }

    #[test]
    fn room_info_titles_fit_the_inner_width() {
        use unicode_width::UnicodeWidthChar;
        let mut app = app();
        app.room_info.state = Some(room_info_with(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
        ));
        for (query, editing) in [
            (String::new(), true),
            ("a".repeat(80), true),
            ("ada".to_owned(), false),
        ] {
            {
                let state = app.room_info.state.as_mut().unwrap();
                state.member_filter.clone_from(&query);
                state.member_filter_editing = editing;
            }
            for width in [0, 1, 8, 24, 80] {
                let title = crate::ui::room_info_popup_title(&app, width);
                let columns: usize = title.chars().map(|ch| ch.width_cjk().unwrap_or(1)).sum();
                assert!(
                    columns <= width,
                    "{title:?} is {columns} columns in a width of {width}"
                );
            }
        }
    }

    #[tokio::test]
    async fn paste_outside_the_room_info_popup_reaches_the_composer() {
        let mut app = app();
        let mut state = room_info_with(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
        );
        state.member_filter_editing = true;
        app.room_info.state = Some(state);
        app.mode = Mode::Compose;
        app.input.buffer.clear();

        app.handle_paste("hi".to_owned());

        assert_eq!(app.input.buffer, "hi");
        assert!(!app.member_filter_is_editing());
        assert!(app
            .room_info
            .state
            .as_ref()
            .unwrap()
            .member_filter
            .is_empty());
    }

    #[tokio::test]
    async fn reopening_whereami_does_not_resume_the_filter_prompt() {
        let mut app = app();
        let mut state = room_info_with(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
        );
        state.member_filter = "ada".to_owned();
        state.member_filter_editing = true;
        app.room_info.state = Some(state);

        app.handle_command(crate::command::Command::Whereami).await;

        assert_eq!(app.mode, Mode::Popup(PopupKind::RoomInfo));
        assert!(!app.member_filter_is_editing());
        assert_eq!(app.room_info.state.as_ref().unwrap().member_filter, "ada");
    }

    #[tokio::test]
    async fn changing_rooms_while_the_popup_is_open_drops_the_prompt() {
        let mut app = app();
        app.room_info.state = Some(room_info_with(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
        ));
        app.room_info.state.as_mut().unwrap().member_filter_editing = true;
        let mut other = room();
        other.room_id = "!other:example.com".to_owned();
        other.name = Some("Other".to_owned());
        app.rooms.rooms.push(other);
        app.mode = Mode::Popup(PopupKind::RoomInfo);

        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
            .await;

        assert!(!app.room_info.state.as_ref().unwrap().member_filter_editing);
        app.handle_key(ctrl('f')).await;
        assert!(!app.member_filter_is_editing());
        assert_eq!(app.mode, Mode::Popup(PopupKind::RoomInfo));
    }

    #[tokio::test]
    async fn a_configured_page_key_scrolls_while_the_filter_prompt_is_open() {
        let mut app = app();
        app.room_info.state = Some(room_info_with(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
        ));
        app.mode = Mode::Popup(PopupKind::RoomInfo);
        app.shortcuts.message_page_down = KeyBinding::from_spec("j");
        app.handle_key(ctrl('f')).await;

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::empty()))
            .await;

        assert_eq!(app.popup_scroll, 8);
        assert!(app
            .room_info
            .state
            .as_ref()
            .unwrap()
            .member_filter
            .is_empty());
        assert!(app.member_filter_is_editing());
    }

    #[tokio::test]
    async fn delete_does_not_bypass_the_backspace_shortcut() {
        let mut app = app();
        app.room_info.state = Some(room_info_with(
            vec![listed_member("join", "@ada:example.com", Some("Ada"))],
            Some(counts(1)),
        ));
        app.mode = Mode::Popup(PopupKind::RoomInfo);
        app.handle_key(ctrl('f')).await;
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty()))
            .await;

        app.handle_key(KeyEvent::from(KeyCode::Delete)).await;
        assert_eq!(app.room_info.state.as_ref().unwrap().member_filter, "a");

        app.handle_key(KeyEvent::from(KeyCode::Backspace)).await;
        assert!(app
            .room_info
            .state
            .as_ref()
            .unwrap()
            .member_filter
            .is_empty());
    }
}
