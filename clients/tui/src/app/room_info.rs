//! Room information for the `/whereami` popup (ADR 0114): the cached-state
//! reads Axon already serves, fetched off the event loop and rendered without
//! turning "Axon does not know" into "the room does not have one".

use chrono::{Local, TimeZone};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use tokio::sync::mpsc;

use super::render::format_time;
use super::{App, RoomKey};
use crate::api::{
    AxonClient, MetadataSnapshot, RoomCreationContent, RoomDto, RoomInfoDto, RoomJoinRulesContent,
    RoomMetadataDto, RoomPowerLevelsContent, RoomUpgradeDto, SnapshotStatus,
};
use crate::config::{ColorScheme, TimeFormat};

/// The three reads behind one `/whereami`.
const ROOM_INFO_READS: u8 = 3;

/// One read's last good value, kept when a later refresh fails so the popup
/// does not go blank over a transient error.
#[derive(Debug)]
pub(crate) struct Part<T> {
    pub(crate) value: Option<T>,
    pub(crate) failed: bool,
}

impl<T> Default for Part<T> {
    fn default() -> Self {
        Self {
            value: None,
            failed: false,
        }
    }
}

impl<T> Part<T> {
    fn settle(&mut self, result: Option<T>) {
        match result {
            Some(value) => {
                self.value = Some(value);
                self.failed = false;
            }
            None => self.failed = true,
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
}

/// Room information for the one room `/whereami` last showed. Holding a single
/// room bounds this cache without an eviction policy; reopening the same room
/// shows its earlier data while the refresh is in flight.
#[derive(Debug, Default)]
pub(crate) struct RoomInfoCache {
    pub(crate) state: Option<RoomInfoState>,
    generation: u64,
    /// `None` until the main loop wires the channel (and in unit tests).
    pub(crate) tx: Option<mpsc::UnboundedSender<RoomInfoOutcome>>,
}

#[derive(Debug)]
pub(crate) enum RoomInfoPart {
    /// Boxed: eight snapshots dwarf the other two reads.
    Metadata(Option<Box<RoomMetadataDto>>),
    Info(Option<RoomInfoDto>),
    Upgrade(Option<RoomUpgradeDto>),
}

/// One completed read; `None` inside the part is a failed request.
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
        cache.generation += 1;
        let generation = cache.generation;
        match cache.state.as_mut().filter(|state| state.key == key) {
            Some(state) => {
                state.generation = generation;
                state.in_flight = ROOM_INFO_READS;
            }
            None => {
                cache.state = Some(RoomInfoState {
                    key: key.clone(),
                    generation,
                    in_flight: ROOM_INFO_READS,
                    metadata: Part::default(),
                    info: Part::default(),
                    upgrade: Part::default(),
                });
            }
        }

        // One task per read, so a slow one does not hold back the others.
        let spawn_read = |read: fn(AxonClient, RoomKey) -> ReadFuture| {
            let (client, tx, key) = (self.client.clone(), tx.clone(), key.clone());
            tokio::spawn(async move {
                let part = read(client, key.clone()).await;
                let _ = tx.send(RoomInfoOutcome {
                    key,
                    generation,
                    part,
                });
            });
        };
        spawn_read(|client, key| {
            Box::pin(async move {
                let read = client.room_metadata(key.account_id, &key.room_id).await;
                RoomInfoPart::Metadata(read.ok().map(Box::new))
            })
        });
        spawn_read(|client, key| {
            Box::pin(async move {
                let read = client.room_info(key.account_id, &key.room_id).await;
                RoomInfoPart::Info(read.ok())
            })
        });
        spawn_read(|client, key| {
            Box::pin(async move {
                let read = client.room_upgrade(key.account_id, &key.room_id).await;
                RoomInfoPart::Upgrade(read.ok())
            })
        });
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
        }
    }

    pub(crate) fn prune_room_info(&mut self, key: &RoomKey) {
        if self
            .room_info
            .state
            .as_ref()
            .is_some_and(|state| &state.key == key)
        {
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
    NotRequested,
}

impl<T> View<'_, T> {
    fn pending_phrase(&self) -> &'static str {
        match self {
            View::Loaded(_) => "",
            View::Loading => "loading…",
            View::Failed => "unavailable (request failed)",
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
        None if part.failed => View::Failed,
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
        Some(counts) => format!(
            "Members: {} joined, {} invited (observed {})",
            counts.joined,
            counts.invited,
            format_datetime(counts.observed_at, time_format)
        ),
        // Null is "not observed yet", never zero.
        None => "Members: unknown (not yet observed)".to_owned(),
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
/// `/metadata`, `/info`, and `/upgrade`.
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
    let stale = state.is_some_and(|state| {
        (state.metadata.failed && state.metadata.value.is_some())
            || (state.info.failed && state.info.value.is_some())
            || (state.upgrade.failed && state.upgrade.value.is_some())
    });
    if stale {
        lines.push(String::new());
        lines.push("Latest refresh failed; some details above may be out of date.".to_owned());
    }
    lines
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

/// Colour one popup line: the field name in the heading colour, and a value
/// that only reports a gap dimmed, so real data stands out from both.
///
/// Only unindented `Label: value` rows and `Heading:` rows are split. Indented
/// rows are list entries whose text (a display name, say) may itself contain
/// a colon.
pub(crate) fn styled_line(line: String, colors: &ColorScheme) -> Line<'static> {
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
    let value_style = match GAP_PREFIXES.iter().any(|gap| value.starts_with(gap)) {
        true => Style::default().fg(colors.input_hint),
        false => Style::default(),
    };
    Line::from(vec![
        Span::styled(format!("{label}: "), label_style),
        Span::styled(value.to_owned(), value_style),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{RoomMemberCountsDto, RoomPredecessorContent};
    use crate::config::TuiConfig;
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
            canonical_alias: Some("#summary:example.com".to_owned()),
            room_type: None,
            last_activity_ts: 0,
            last_event_id: None,
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
                failed: false,
            },
            info: Part {
                value: info,
                failed: false,
            },
            upgrade: Part {
                value: upgrade,
                failed: false,
            },
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
                member_counts: Some(RoomMemberCountsDto {
                    joined: 412,
                    invited: 3,
                    observed_at: 1_700_000_000_000,
                }),
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
        assert!(line(&lines, "Members:").starts_with("Members: 412 joined, 3 invited (observed "));
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
                member_counts: None,
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
        let info: RoomInfoDto = serde_json::from_value(json!({
            "member_counts": { "joined": 2, "invited": 0, "observed_at": 5 },
        }))
        .unwrap();
        assert_eq!(info.member_counts.map(|counts| counts.joined), Some(2));

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
    async fn whereami_reads_all_three_endpoints_over_http() {
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
            ["info", "metadata", "upgrade"].map(|endpoint| format!("{base}/{endpoint}"))
        );
        let lines = crate::ui::popup_room_info_lines(&app);
        assert_eq!(
            line(&lines, "Encryption:"),
            "Encryption: m.megolm.v1.aes-sha2"
        );
        assert_eq!(line(&lines, "Room version:"), "Room version: 11");
        assert!(line(&lines, "Members:").starts_with("Members: 412 joined, 3 invited"));
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
    fn stale_and_superseded_results_are_dropped() {
        let mut app = app();
        let mut state = loaded(None, None, None);
        state.generation = 2;
        state.in_flight = ROOM_INFO_READS;
        app.room_info.state = Some(state);
        let info = || RoomInfoPart::Info(Some(RoomInfoDto::default()));

        let mut other = key();
        other.room_id = "!other:example.com".to_owned();
        app.apply_room_info_outcome(RoomInfoOutcome {
            key: other,
            generation: 2,
            part: info(),
        });
        app.apply_room_info_outcome(RoomInfoOutcome {
            key: key(),
            generation: 1,
            part: info(),
        });
        let state = app.room_info.state.as_ref().unwrap();
        assert!(state.info.value.is_none());
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
            part: RoomInfoPart::Metadata(None),
        });

        let lines = crate::ui::popup_room_info_lines(&app);
        assert_eq!(line(&lines, "Room version:"), "Room version: 11");
        assert!(lines
            .iter()
            .any(|line| line.starts_with("Latest refresh failed")));
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
}
