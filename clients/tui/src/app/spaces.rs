//! Shallow space projection and instance order (ADR 0103).
//! The main loop owns every map. Workers return keyed, versioned outcomes;
//! neither key handling nor the renderer waits for network work.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, Semaphore};
use tokio::task::AbortHandle;

use super::{App, BootstrapStage, RoomFilter, RoomKey, Status};
use crate::api::{
    ApiError, EventDto, MemberDto, PreferenceDto, PreferencesChangedDto, RoomDto, SpaceChildDto,
    TimelinePage,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const WORKERS: usize = 4;
const LOOKAHEAD: usize = 8;
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SidebarRow {
    Space {
        index: usize,
        expanded: bool,
    },
    Room {
        index: usize,
        indented: bool,
        number: usize,
    },
    Ungrouped {
        provisional: bool,
    },
    Divider,
}

impl SidebarRow {
    pub(crate) fn index(&self) -> Option<usize> {
        match self {
            Self::Space { index, .. } | Self::Room { index, .. } => Some(*index),
            _ => None,
        }
    }

    pub(crate) fn height(&self, wide: bool) -> usize {
        if !wide && matches!(self, Self::Room { .. }) {
            2
        } else {
            1
        }
    }
}

/// The section containing the first visible child or divider. A section
/// heading already in the viewport does not need another copy above it.
pub(crate) fn sidebar_section_header(rows: &[SidebarRow], start: usize) -> Option<usize> {
    if !matches!(
        rows.get(start),
        Some(SidebarRow::Room { .. } | SidebarRow::Divider)
    ) {
        return None;
    }
    rows[..start]
        .iter()
        .rposition(|row| matches!(row, SidebarRow::Space { .. } | SidebarRow::Ungrouped { .. }))
}

#[derive(Default)]
pub(crate) struct ChildrenState {
    pub(crate) children: Option<Vec<String>>,
    pub(crate) error: Option<String>,
    dirty: bool,
    inflight: Option<u64>,
    abort: Option<AbortHandle>,
    retry_at: Option<Instant>,
}

pub(crate) struct SpacesState {
    /// A header can have focus while `rooms.selected` keeps the open timeline.
    pub(crate) focus: Option<RoomKey>,
    pub(crate) collapsed: HashSet<RoomKey>,
    /// Explicit collapse choices made while a filter is revealing matches.
    pub(crate) filter_collapsed: HashSet<RoomKey>,
    pub(crate) children: HashMap<RoomKey, ChildrenState>,
    pub(crate) order: Vec<RoomKey>,
    pub(crate) order_error: Option<String>,
    pub(crate) tx: Option<mpsc::UnboundedSender<SpaceOutcome>>,
    workers: Arc<Semaphore>,
    next_request: u64,
    timeline: Option<PendingTimeline>,
    order_ready: bool,
    order_revision: u64,
    order_dirty: bool,
    order_inflight: bool,
    order_read_again: bool,
    order_retry_at: Option<Instant>,
    order_failures: u32,
    launch_deadline: Option<Instant>,
    launch_started: bool,
    navigation: Option<SpaceNavigation>,
}

impl Default for SpacesState {
    fn default() -> Self {
        Self {
            focus: None,
            collapsed: HashSet::new(),
            filter_collapsed: HashSet::new(),
            children: HashMap::new(),
            order: Vec::new(),
            order_error: None,
            tx: None,
            workers: Arc::new(Semaphore::new(WORKERS)),
            next_request: 0,
            timeline: None,
            order_ready: false,
            order_revision: 0,
            order_dirty: false,
            order_inflight: false,
            order_read_again: true,
            order_retry_at: None,
            order_failures: 0,
            launch_deadline: None,
            launch_started: false,
            navigation: None,
        }
    }
}

enum LaunchSelection {
    Waiting(Option<RoomKey>),
    Ready(Option<usize>),
}

#[derive(Clone)]
struct SpaceNavigation {
    key: RoomKey,
    advance_if_empty: bool,
}

struct PendingTimeline {
    request: u64,
    key: RoomKey,
    abort: AbortHandle,
    live: Vec<EventDto>,
    overflow: bool,
}

pub(crate) struct SidebarTimelineOutcome {
    room: RoomDto,
    request: u64,
    page: Result<TimelinePage, ApiError>,
    members: Result<Vec<MemberDto>, ApiError>,
}

pub(crate) enum SpaceOutcome {
    Timeline(Box<SidebarTimelineOutcome>),
    Children {
        key: RoomKey,
        request: u64,
        result: Result<Vec<SpaceChildDto>, ApiError>,
    },
    OrderRead {
        revision: u64,
        result: Result<PreferenceDto, ApiError>,
    },
    OrderWrite {
        revision: u64,
        result: Result<(), ApiError>,
    },
}

impl RoomKey {
    pub(crate) fn preference_entry(&self) -> String {
        format!("{}/{}", self.account_id, self.room_id)
    }
}

fn parse_order(value: serde_json::Value) -> Result<Vec<RoomKey>, String> {
    if serde_json::to_vec(&value)
        .map_err(|_| "invalid space order")?
        .len()
        > 64 * 1024
    {
        return Err("space order exceeds 64 KiB".to_owned());
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Order {
        spaces: Vec<String>,
    }
    let parsed: Order =
        serde_json::from_value(value).map_err(|_| "invalid space order".to_owned())?;
    let mut seen = HashSet::new();
    let mut keys = Vec::new();
    for entry in parsed.spaces {
        let (account, room_id) = entry.split_once('/').ok_or("invalid space order key")?;
        let account_id =
            uuid::Uuid::parse_str(account).map_err(|_| "invalid space order account")?;
        if !room_id.starts_with('!') || room_id.len() <= 1 {
            return Err("invalid space order room".to_owned());
        }
        let key = RoomKey {
            account_id,
            room_id: room_id.to_owned(),
        };
        if seen.insert(key.clone()) {
            keys.push(key);
        }
    }
    Ok(keys)
}

impl App {
    pub(crate) fn space_launch_pending(&self) -> bool {
        self.spaces.launch_deadline.is_some()
    }

    pub(crate) fn begin_space_launch(&mut self) {
        if self.spaces.launch_started || !self.space_tree_enabled() {
            return;
        }
        self.spaces.launch_started = true;
        self.spaces.launch_deadline = Some(Instant::now() + LAUNCH_TIMEOUT);
        self.rooms.selected = None;
        self.rooms.scroll = 0;
        self.spaces.focus = self
            .ordered_space_indices(self.active_account_filter())
            .first()
            .map(|&index| RoomKey::from(&self.rooms.rooms[index]));
    }

    fn launch_selection(&self) -> LaunchSelection {
        if self
            .eligible_room_indices(self.active_account_filter())
            .is_empty()
        {
            return LaunchSelection::Ready(None);
        }
        if self.space_tree_enabled()
            && !self.spaces.order_ready
            && self.spaces.order_error.is_none()
        {
            return LaunchSelection::Waiting(None);
        }
        for row in self.sidebar_rows(self.active_account_filter()) {
            match row {
                SidebarRow::Space { index, .. } => {
                    let key = RoomKey::from(&self.rooms.rooms[index]);
                    if self
                        .spaces
                        .children
                        .get(&key)
                        .is_none_or(|state| state.children.is_none() && state.error.is_none())
                    {
                        return LaunchSelection::Waiting(Some(key));
                    }
                }
                SidebarRow::Room { index, .. } => return LaunchSelection::Ready(Some(index)),
                _ => {}
            }
        }
        LaunchSelection::Ready(None)
    }

    fn cancel_space_launch(&mut self) {
        self.spaces.launch_started = true;
        self.spaces.launch_deadline = None;
        if self.bootstrap == BootstrapStage::Spaces {
            self.note_stage_elapsed("space selection");
            self.bootstrap = BootstrapStage::Done;
        }
    }

    pub(crate) fn finish_space_launch(&mut self, now: Instant) {
        if self.bootstrap != BootstrapStage::Spaces {
            return;
        }
        let Some(deadline) = self.spaces.launch_deadline else {
            return;
        };
        let index = match self.launch_selection() {
            LaunchSelection::Ready(index) => index,
            LaunchSelection::Waiting(_) if now >= deadline => {
                self.visible_room_indices().first().copied()
            }
            LaunchSelection::Waiting(_) => return,
        };
        self.cancel_space_launch();
        if let Some(index) = index {
            self.activate_sidebar_room(index);
        }
    }

    pub(crate) fn ordered_space_indices(&self, account: Option<uuid::Uuid>) -> Vec<usize> {
        let ranks: HashMap<&RoomKey, usize> = self
            .spaces
            .order
            .iter()
            .enumerate()
            .map(|(i, k)| (k, i))
            .collect();
        let mut roots: Vec<_> = self
            .rooms
            .rooms
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.room_type.as_deref() == Some("m.space")
                    && account.is_none_or(|a| a == r.account_id)
            })
            .map(|(i, _)| i)
            .collect();
        roots.sort_by_cached_key(|&i| {
            let room = &self.rooms.rooms[i];
            let key = RoomKey::from(room);
            (
                ranks.get(&key).copied().unwrap_or(usize::MAX),
                self.room_list_title(room).to_lowercase(),
                key.preference_entry(),
            )
        });
        roots
    }

    pub(crate) fn space_tree_enabled(&self) -> bool {
        self.room_filter != RoomFilter::Favorites
            && !self
                .ordered_space_indices(self.active_account_filter())
                .is_empty()
    }

    /// Eligible targets include collapsed children. Numbered targets and next/
    /// previous room navigation instead use the projected visible leaf rows.
    pub(crate) fn eligible_room_indices(&self, account: Option<uuid::Uuid>) -> Vec<usize> {
        self.rooms
            .rooms
            .iter()
            .enumerate()
            .filter(|(_, r)| account.is_none_or(|a| a == r.account_id))
            .filter(|(_, r)| r.room_type.as_deref() != Some("m.space"))
            .filter(|(i, r)| self.rooms.selected == Some(*i) || self.room_passes_filter(r))
            .map(|(i, _)| i)
            .collect()
    }

    pub(crate) fn sidebar_rows(&self, account: Option<uuid::Uuid>) -> Vec<SidebarRow> {
        let roots = self.ordered_space_indices(account);
        let candidates = self.eligible_room_indices(account);
        let mut rows = Vec::new();
        if roots.is_empty() || self.room_filter == RoomFilter::Favorites {
            self.append_room_rows(&mut rows, &candidates, false);
            return rows;
        }
        // Ownership is independent of collapse state, filters and HTTP completion
        // order: the first root in the canonical display order owns each child.
        let mut owner = HashMap::new();
        let mut provisional = false;
        for &index in &roots {
            let key = RoomKey::from(&self.rooms.rooms[index]);
            let state = self.spaces.children.get(&key);
            provisional |=
                state.is_none_or(|s| s.children.is_none() || s.dirty || s.error.is_some());
            if let Some(children) = state.and_then(|s| s.children.as_ref()) {
                for child in children {
                    owner
                        .entry((key.account_id, child.as_str()))
                        .or_insert(index);
                }
            }
        }
        let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut ungrouped = Vec::new();
        for index in candidates {
            let room = &self.rooms.rooms[index];
            match owner.get(&(room.account_id, room.room_id.as_str())) {
                Some(parent) => groups.entry(*parent).or_default().push(index),
                None => ungrouped.push(index),
            }
        }
        for index in roots {
            let room = &self.rooms.rooms[index];
            let key = RoomKey::from(room);
            let children = groups.remove(&index).unwrap_or_default();
            let loading = self
                .spaces
                .children
                .get(&key)
                .is_none_or(|s| s.children.is_none());
            if self.room_filter != RoomFilter::All
                && children.is_empty()
                && !loading
                && !self.room_passes_filter(room)
            {
                continue;
            }
            let reveal = matches!(self.room_filter, RoomFilter::Name(_) | RoomFilter::Unread)
                && !children.is_empty()
                && !self.spaces.filter_collapsed.contains(&key);
            let expanded = reveal || !self.spaces.collapsed.contains(&key);
            rows.push(SidebarRow::Space { index, expanded });
            if expanded {
                self.append_room_rows(&mut rows, &children, true);
            }
        }
        if !ungrouped.is_empty() {
            rows.push(SidebarRow::Ungrouped { provisional });
            self.append_room_rows(&mut rows, &ungrouped, false);
        }
        let mut number = 0;
        for row in &mut rows {
            if let SidebarRow::Room { number: n, .. } = row {
                number += 1;
                *n = number;
            }
        }
        rows
    }

    fn append_room_rows(&self, rows: &mut Vec<SidebarRow>, indices: &[usize], indented: bool) {
        let mut had_favorite = false;
        let mut divided = false;
        for (number, &index) in indices.iter().enumerate() {
            let favorite = self.is_room_pinned(&self.rooms.rooms[index]);
            if had_favorite && !favorite && !divided {
                rows.push(SidebarRow::Divider);
                divided = true;
            }
            had_favorite |= favorite;
            rows.push(SidebarRow::Room {
                index,
                indented,
                number: number + 1,
            });
        }
    }

    pub(crate) fn focused_sidebar_index(&self) -> Option<usize> {
        self.spaces
            .focus
            .as_ref()
            .and_then(|key| {
                self.rooms
                    .rooms
                    .iter()
                    .position(|r| RoomKey::from(r) == *key)
            })
            .or(self.rooms.selected)
    }

    pub(crate) fn activate_sidebar_room(&mut self, index: usize) {
        let defer_until_markers = matches!(
            self.bootstrap,
            BootstrapStage::Rooms | BootstrapStage::DeviceState
        );
        self.cancel_space_launch();
        self.spaces.navigation = None;
        self.rooms.selected = Some(index);
        self.reveal_room_parent(index);
        self.messages.selection = None;
        self.messages.scroll = usize::MAX;
        self.last_jump_ts = None;
        self.force_terminal_clear = true;
        self.thread_panel = None;
        if let Some(pending) = self.spaces.timeline.take() {
            pending.abort.abort();
        }
        if defer_until_markers {
            return;
        }
        self.sync_draft_on_room_change();
        let Some(tx) = self.spaces.tx.clone() else {
            return;
        };
        let room = self.rooms.rooms[index].clone();
        let room_key = RoomKey::from(&room);
        let client = self.client.clone();
        self.spaces.next_request += 1;
        let request = self.spaces.next_request;
        let task = tokio::spawn(async move {
            let (page, members) = tokio::join!(
                client.room_timeline(
                    room.account_id,
                    &room.room_id,
                    None,
                    None,
                    super::TIMELINE_LIMIT
                ),
                client.room_members(room.account_id, &room.room_id),
            );
            let _ = tx.send(SpaceOutcome::Timeline(Box::new(SidebarTimelineOutcome {
                room,
                request,
                page,
                members,
            })));
        });
        self.spaces.timeline = Some(PendingTimeline {
            request,
            key: room_key,
            abort: task.abort_handle(),
            live: Vec::new(),
            overflow: false,
        });
    }

    /// The HTTP snapshot can predate live events observed while it was in
    /// flight. Replay those frames through the ordinary event handler after
    /// installing the snapshot, including edits and reaction patches. If the
    /// bounded buffer fills, keep the live cache and refetch instead.
    pub(crate) fn queue_sidebar_live_event(&mut self, event: &EventDto) {
        let Some(pending) = self.spaces.timeline.as_mut() else {
            return;
        };
        if pending.key.account_id != event.account_id || pending.key.room_id != event.room_id {
            return;
        }
        if pending.live.len() >= 1000 {
            pending.overflow = true;
        } else {
            pending.live.push(event.clone());
        }
    }

    /// Headers are focus targets; section headings and dividers are skipped.
    pub(crate) fn handle_space_list_key(&mut self, key: KeyEvent) -> bool {
        if self.shortcuts.move_space_up.matches(key) {
            self.move_focused_space(-1);
            return true;
        }
        if self.shortcuts.move_space_down.matches(key) {
            self.move_focused_space(1);
            return true;
        }
        if (self.shortcuts.toggle_space.matches(key) || self.shortcuts.submit.matches(key))
            && self.toggle_focused_space()
        {
            return true;
        }
        if self.spaces.focus.is_some() {
            if self.shortcuts.pin_room.matches(key) {
                self.pin_focused_space();
                return true;
            }
            if self.shortcuts.unpin_room.matches(key) {
                self.status = Status::from(format!(
                    "spaces use ordering; move them with {} / {}",
                    self.shortcuts.move_space_up.label(),
                    self.shortcuts.move_space_down.label()
                ));
                return true;
            }
        }
        if key.modifiers != KeyModifiers::NONE {
            return false;
        }
        if matches!(key.code, KeyCode::Left | KeyCode::Right) {
            self.cancel_space_launch();
            self.spaces.navigation = None;
            let parent = self.spaces.focus.clone().or_else(|| {
                self.rooms
                    .selected
                    .and_then(|index| self.room_parent_key(index))
            });
            if let Some(parent) = parent {
                if key.code == KeyCode::Left {
                    self.set_space_expanded(&parent, false);
                    self.navigate_after_space(&parent);
                } else if self.spaces.focus.is_some() || self.spaces.collapsed.contains(&parent) {
                    self.open_space_first_room(parent, false);
                }
            }
            return true;
        }
        let offset = match key.code {
            KeyCode::Up => -1,
            KeyCode::Down => 1,
            KeyCode::PageUp => -(self.rooms.page_size.max(1) as isize),
            KeyCode::PageDown => self.rooms.page_size.max(1) as isize,
            KeyCode::Home => isize::MIN,
            KeyCode::End => isize::MAX,
            _ => return false,
        };
        self.cancel_space_launch();
        self.spaces.navigation = None;
        let rows = self.sidebar_rows(self.active_account_filter());
        let mut line = 0;
        let selectable: Vec<_> = rows
            .iter()
            .filter_map(|row| {
                let item = row.index().map(|index| (index, line));
                line += row.height(self.frame.areas.rooms_wide);
                item
            })
            .collect();
        if selectable.is_empty() {
            return true;
        }
        let position = selectable
            .iter()
            .position(|(index, _)| Some(*index) == self.focused_sidebar_index())
            .unwrap_or(0);
        let next = if offset == isize::MIN {
            0
        } else if offset == isize::MAX {
            selectable.len() - 1
        } else if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            let target = selectable[position].1.saturating_add_signed(offset);
            if offset > 0 {
                selectable
                    .iter()
                    .position(|(_, line)| *line >= target)
                    .unwrap_or(selectable.len() - 1)
            } else {
                selectable
                    .iter()
                    .rposition(|(_, line)| *line <= target)
                    .unwrap_or(0)
            }
        } else {
            position
                .saturating_add_signed(offset)
                .min(selectable.len() - 1)
        };
        let index = selectable[next].0;
        if self.rooms.rooms[index].room_type.as_deref() == Some("m.space") {
            self.spaces.focus = Some(RoomKey::from(&self.rooms.rooms[index]));
        } else {
            self.activate_sidebar_room(index);
        }
        true
    }

    pub(crate) fn toggle_focused_space(&mut self) -> bool {
        let Some(key) = self.spaces.focus.clone() else {
            return false;
        };
        self.cancel_space_launch();
        self.spaces.navigation = None;
        let expanded = self.sidebar_rows(self.active_account_filter()).iter().any(|row| {
            matches!(row, SidebarRow::Space { index, expanded: true } if RoomKey::from(&self.rooms.rooms[*index]) == key)
        });
        self.set_space_expanded(&key, !expanded);
        true
    }

    fn set_space_expanded(&mut self, key: &RoomKey, expanded: bool) {
        if expanded {
            self.spaces.collapsed.remove(key);
            self.spaces.filter_collapsed.remove(key);
        } else {
            self.spaces.collapsed.insert(key.clone());
            if matches!(self.room_filter, RoomFilter::Name(_) | RoomFilter::Unread) {
                self.spaces.filter_collapsed.insert(key.clone());
            }
        }
    }

    fn first_space_room(&self, key: &RoomKey) -> Option<usize> {
        let rows = self.sidebar_rows(self.active_account_filter());
        let start = rows.iter().position(|row| {
            matches!(row, SidebarRow::Space { index, .. } if RoomKey::from(&self.rooms.rooms[*index]) == *key)
        })?;
        rows[start + 1..]
            .iter()
            .take_while(|row| {
                !matches!(row, SidebarRow::Space { .. } | SidebarRow::Ungrouped { .. })
            })
            .find_map(|row| match row {
                SidebarRow::Room { index, .. } => Some(*index),
                _ => None,
            })
    }

    /// Returns false for an empty/failed group. An unloaded group retains a
    /// keyed navigation intent, canceled by the next explicit navigation.
    fn open_space_first_room(&mut self, key: RoomKey, advance_if_empty: bool) -> bool {
        self.set_space_expanded(&key, true);
        self.spaces.focus = Some(key.clone());
        if let Some(index) = self.first_space_room(&key) {
            self.activate_sidebar_room(index);
        } else if self
            .spaces
            .children
            .get(&key)
            .is_none_or(|state| state.children.is_none() && state.error.is_none())
        {
            self.spaces.navigation = Some(SpaceNavigation {
                key,
                advance_if_empty,
            });
        } else {
            return false;
        }
        true
    }

    fn navigate_after_space(&mut self, key: &RoomKey) {
        let rows = self.sidebar_rows(self.active_account_filter());
        let Some(start) = rows.iter().position(|row| {
            matches!(row, SidebarRow::Space { index, .. } if RoomKey::from(&self.rooms.rooms[*index]) == *key)
        }) else {
            return;
        };
        for row in &rows[start + 1..] {
            match row {
                SidebarRow::Space { index, .. } => {
                    if self.open_space_first_room(RoomKey::from(&self.rooms.rooms[*index]), true) {
                        return;
                    }
                }
                SidebarRow::Room { index, .. } => {
                    self.activate_sidebar_room(*index);
                    return;
                }
                _ => {}
            }
        }
        self.spaces.focus = Some(key.clone());
        self.status = Status::from("space collapsed; no following room");
    }

    fn finish_space_navigation(&mut self) {
        let Some(navigation) = self.spaces.navigation.clone() else {
            return;
        };
        if self.spaces.focus.as_ref() != Some(&navigation.key) || !self.space_tree_enabled() {
            self.spaces.navigation = None;
            return;
        }
        if self
            .spaces
            .children
            .get(&navigation.key)
            .is_none_or(|state| state.children.is_none() && state.error.is_none())
        {
            return;
        }
        self.spaces.navigation = None;
        if let Some(index) = self.first_space_room(&navigation.key) {
            self.activate_sidebar_room(index);
        } else if navigation.advance_if_empty {
            self.navigate_after_space(&navigation.key);
        }
    }

    pub(crate) fn cancel_space_navigation(&mut self) {
        self.spaces.navigation = None;
    }

    pub(crate) fn room_parent_key(&self, index: usize) -> Option<RoomKey> {
        let room = &self.rooms.rooms[index];
        self.ordered_space_indices(self.active_account_filter())
            .into_iter()
            .map(|parent| RoomKey::from(&self.rooms.rooms[parent]))
            .find(|key| {
                key.account_id == room.account_id
                    && self
                        .spaces
                        .children
                        .get(key)
                        .and_then(|s| s.children.as_ref())
                        .is_some_and(|children| children.contains(&room.room_id))
            })
    }

    pub(crate) fn reveal_room_parent(&mut self, index: usize) {
        self.spaces.focus = None;
        if let Some(key) = self.room_parent_key(index) {
            self.set_space_expanded(&key, true);
        }
    }

    pub(crate) fn reconcile_spaces(&mut self) {
        let present: HashSet<_> = self
            .ordered_space_indices(None)
            .into_iter()
            .map(|i| RoomKey::from(&self.rooms.rooms[i]))
            .collect();
        self.spaces.children.retain(|key, state| {
            if present.contains(key) {
                true
            } else {
                if let Some(task) = state.abort.take() {
                    task.abort();
                }
                false
            }
        });
        self.spaces.collapsed.retain(|key| present.contains(key));
        self.spaces
            .filter_collapsed
            .retain(|key| present.contains(key));
        if self.spaces.focus.as_ref().is_some_and(|key| {
            !present.contains(key)
                || self
                    .active_account_filter()
                    .is_some_and(|a| a != key.account_id)
        }) {
            self.spaces.focus = None;
        }
    }

    pub(crate) fn invalidate_space(&mut self, key: &RoomKey) {
        if let Some(state) = self.spaces.children.get_mut(key) {
            state.dirty = true;
            state.retry_at = None;
        }
    }

    pub(crate) fn refresh_spaces(&mut self) {
        for state in self.spaces.children.values_mut() {
            state.dirty = true;
            state.retry_at = None;
        }
        self.spaces.order_read_again = true;
        self.spaces.order_retry_at = None;
    }

    /// Only roots on screen plus lookahead acquire worker permits. No task is
    /// spawned to wait for a permit, keeping both active work and queues bounded.
    pub(crate) fn sweep_spaces(&mut self, now: Instant) {
        self.finish_space_launch(now);
        self.finish_space_navigation();
        self.sweep_space_order(now);
        if !self.space_tree_enabled()
            || (!self.rooms_panel_visible() && !self.space_launch_pending())
        {
            return;
        }
        let Some(tx) = self.spaces.tx.clone() else {
            return;
        };
        let rows = self.sidebar_rows(self.active_account_filter());
        let start = self.rooms.scroll.saturating_sub(LOOKAHEAD);
        let end = self
            .rooms
            .scroll
            .saturating_add(self.rooms.page_size)
            .saturating_add(LOOKAHEAD)
            .min(rows.len());
        // The containing root can be above the lookahead window while its
        // children remain visible. Keep its membership fresh as well.
        let header = sidebar_section_header(&rows, self.rooms.scroll);
        let launch_root = if self.space_launch_pending() {
            match self.launch_selection() {
                LaunchSelection::Waiting(root) => root,
                LaunchSelection::Ready(_) => None,
            }
        } else {
            None
        };
        let navigation_root = self
            .spaces
            .navigation
            .as_ref()
            .map(|navigation| navigation.key.clone());
        let keys: Vec<_> = launch_root
            .into_iter()
            .chain(navigation_root)
            .chain(
                header
                    .map(|index| &rows[index])
                    .into_iter()
                    .chain(rows[start.min(end)..end].iter())
                    .filter_map(|row| {
                        if let SidebarRow::Space { index, .. } = row {
                            Some(RoomKey::from(&self.rooms.rooms[*index]))
                        } else {
                            None
                        }
                    }),
            )
            .collect();
        for key in keys {
            let state = self.spaces.children.entry(key.clone()).or_default();
            if state.inflight.is_some()
                || state.retry_at.is_some_and(|at| now < at)
                || (state.children.is_some() && !state.dirty)
            {
                continue;
            }
            let Ok(permit) = self.spaces.workers.clone().try_acquire_owned() else {
                break;
            };
            self.spaces.next_request += 1;
            let request = self.spaces.next_request;
            state.inflight = Some(request);
            state.dirty = false;
            let client = self.client.clone();
            let tx = tx.clone();
            let task = tokio::spawn(async move {
                let result = client.space_children(key.account_id, &key.room_id).await;
                let _ = tx.send(SpaceOutcome::Children {
                    key,
                    request,
                    result,
                });
                drop(permit);
            });
            state.abort = Some(task.abort_handle());
        }
    }

    fn sweep_space_order(&mut self, now: Instant) {
        if self.spaces.order_inflight || self.spaces.order_retry_at.is_some_and(|at| now < at) {
            return;
        }
        if !self.spaces.order_dirty && !self.spaces.order_read_again {
            return;
        }
        // Avoid a preference request on accounts with no spaces at all.
        if self.ordered_space_indices(None).is_empty() {
            return;
        }
        let Some(tx) = self.spaces.tx.clone() else {
            return;
        };
        self.spaces.order_inflight = true;
        let revision = self.spaces.order_revision;
        let client = self.client.clone();
        if self.spaces.order_dirty {
            let entries: Vec<_> = self
                .spaces
                .order
                .iter()
                .map(RoomKey::preference_entry)
                .collect();
            let device_id = self.device_id;
            tokio::spawn(async move {
                let result = client.put_space_order(device_id, &entries).await;
                let _ = tx.send(SpaceOutcome::OrderWrite { revision, result });
            });
        } else {
            self.spaces.order_read_again = false;
            tokio::spawn(async move {
                let result = client.space_order().await;
                let _ = tx.send(SpaceOutcome::OrderRead { revision, result });
            });
        }
    }

    pub(crate) fn move_focused_space(&mut self, offset: isize) {
        let Some(key) = self.spaces.focus.clone() else {
            return;
        };
        if !self.spaces.order_ready {
            self.status = Status::from("space order is still loading; try again");
            return;
        }
        let mut visible: Vec<_> = self
            .ordered_space_indices(self.active_account_filter())
            .into_iter()
            .map(|i| RoomKey::from(&self.rooms.rooms[i]))
            .collect();
        let Some(index) = visible.iter().position(|k| *k == key) else {
            return;
        };
        let next = index
            .saturating_add_signed(offset)
            .min(visible.len().saturating_sub(1));
        if next == index {
            return;
        }
        visible.swap(index, next);
        // Replace only the visible slots. Other accounts retain their rank;
        // temporarily absent keys are never discarded by a filtered move.
        let visible_keys: HashSet<_> = visible.iter().cloned().collect();
        let mut replacements = visible.iter();
        let mut order = Vec::new();
        for old in &self.spaces.order {
            if visible_keys.contains(old) {
                if let Some(key) = replacements.next() {
                    order.push(key.clone());
                }
            } else {
                order.push(old.clone());
            }
        }
        order.extend(replacements.cloned());
        self.spaces.order = order;
        self.space_order_edited();
    }

    fn space_order_edited(&mut self) {
        self.spaces.order_revision += 1;
        self.spaces.order_dirty = true;
        self.spaces.order_retry_at = None;
        self.status = Status::from("saving space order…");
    }

    fn pin_focused_space(&mut self) {
        let Some(key) = self.spaces.focus.clone() else {
            return;
        };
        if !self.spaces.order_ready {
            self.status = Status::from("space order is still loading; try again");
            return;
        }
        self.cancel_space_launch();
        let roots: Vec<_> = self
            .ordered_space_indices(None)
            .into_iter()
            .map(|index| RoomKey::from(&self.rooms.rooms[index]))
            .collect();
        if !roots.contains(&key) {
            return;
        }
        let mut order = vec![key.clone()];
        order.extend(self.spaces.order.iter().filter(|old| **old != key).cloned());
        let mut seen: HashSet<_> = order.iter().cloned().collect();
        order.extend(roots.into_iter().filter(|root| seen.insert(root.clone())));
        if order != self.spaces.order {
            self.spaces.order = order;
            self.space_order_edited();
        }
    }

    pub(crate) fn apply_preferences_changed(&mut self, frame: PreferencesChangedDto) {
        if frame.key != "space_order" || frame.device_id == self.device_id {
            return;
        }
        let Ok(order) = parse_order(frame.value) else {
            return;
        };
        if self.spaces.order_dirty {
            self.spaces.order_read_again = true;
            return;
        }
        self.spaces.order = order;
        self.spaces.order_ready = true;
        self.spaces.order_revision += 1;
        self.spaces.order_error = None;
        self.spaces.order_retry_at = None;
        self.spaces.order_failures = 0;
    }

    pub(crate) fn apply_space_outcome(&mut self, outcome: SpaceOutcome) {
        match outcome {
            SpaceOutcome::Timeline(outcome) => {
                if self.spaces.timeline.as_ref().map(|pending| pending.request)
                    != Some(outcome.request)
                {
                    return;
                }
                let pending = self
                    .spaces
                    .timeline
                    .take()
                    .expect("checked pending timeline");
                if self.selected_room().map(RoomKey::from) != Some(RoomKey::from(&outcome.room))
                    || self.last_jump_ts.is_some()
                    || self.thread_panel.is_some()
                {
                    return;
                }
                if pending.overflow {
                    if let Some(index) = self.rooms.selected {
                        self.activate_sidebar_room(index);
                    }
                    return;
                }
                match outcome.page {
                    Ok(page) => {
                        self.apply_timeline_page(
                            &outcome.room,
                            page,
                            outcome.members.ok().as_deref(),
                        );
                        for event in pending.live {
                            self.handle_live_frame(crate::api::LiveFrame::Timeline(Box::new(
                                event,
                            )));
                        }
                    }
                    Err(err) => {
                        if !self.is_mid_command() {
                            self.status = Status::from(format!("timeline load failed: {err}"));
                        }
                    }
                }
            }
            SpaceOutcome::Children {
                key,
                request,
                result,
            } => {
                let report_debug = self.display.debug && !self.is_mid_command();
                let Some(state) = self.spaces.children.get_mut(&key) else {
                    return;
                };
                if state.inflight != Some(request) {
                    return;
                }
                state.inflight = None;
                state.abort = None;
                // A live invalidation during a GET requires another GET; the
                // first response may predate a removal and cannot settle it.
                if state.dirty {
                    return;
                }
                match result {
                    Ok(children) => {
                        state.children = Some(children.into_iter().map(|c| c.room_id).collect());
                        state.error = None;
                        state.retry_at = None;
                    }
                    Err(err) => {
                        let error = err.to_string();
                        state.error = Some(error.clone());
                        if report_debug {
                            self.status = Status::Debug(format!(
                                "space children failed: account_id={} room_id={}: {error}",
                                key.account_id, key.room_id
                            ));
                        }
                        state.dirty = true;
                        state.retry_at = Some(Instant::now() + Duration::from_secs(30));
                    }
                }
            }
            SpaceOutcome::OrderRead { revision, result } => {
                self.spaces.order_inflight = false;
                if revision != self.spaces.order_revision || self.spaces.order_dirty {
                    return;
                }
                match result {
                    Ok(preference) => match parse_order(preference.value) {
                        Ok(order) => {
                            self.spaces.order = order;
                            self.spaces.order_ready = true;
                            self.spaces.order_error = None;
                            self.spaces.order_failures = 0;
                        }
                        Err(error) => self.space_order_failed(error),
                    },
                    Err(err) if err.is_not_found() => {
                        self.spaces.order.clear();
                        self.spaces.order_ready = true;
                        self.spaces.order_error = None;
                    }
                    Err(err) => self.space_order_failed(err.to_string()),
                }
            }
            SpaceOutcome::OrderWrite { revision, result } => {
                self.spaces.order_inflight = false;
                match result {
                    Ok(()) => {
                        if revision == self.spaces.order_revision {
                            self.spaces.order_dirty = false;
                            self.spaces.order_error = None;
                            self.spaces.order_failures = 0;
                            self.spaces.order_retry_at = None;
                            if !self.is_mid_command() {
                                self.status = Status::from("saved space order");
                            }
                        }
                    }
                    Err(err) => self.space_order_failed(err.to_string()),
                }
            }
        }
    }

    fn space_order_failed(&mut self, error: String) {
        self.spaces.order_error = Some(error);
        self.spaces.order_read_again = true;
        self.spaces.order_failures = self.spaces.order_failures.saturating_add(1);
        let seconds = (1u64 << self.spaces.order_failures.saturating_sub(1).min(5)).min(30);
        self.spaces.order_retry_at = Some(Instant::now() + Duration::from_secs(seconds));
        if !self.is_mid_command() {
            self.status = Status::from("could not save/load space order; retrying");
        }
    }
}

#[cfg(test)]
mod tests;
