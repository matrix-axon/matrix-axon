use std::collections::HashMap;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::api::{
    EventDto, LiveFrame, MemberDto, RoomDto, SenderTrustViolationDto, VerificationFrame,
    VerificationFrameKind,
};
use crate::config::{DisplayOptions, MessageDensity};

use ratatui_image::Resize;

use super::relations::thread_visible;
use super::{
    collect_reactions, match_status, message_index_at_line, next_match_index,
    selected_message_target_index, App, ConnectionState, ImageState, ImageThumbRows,
    LiveFrameAction, MediaKey, RoomKey, RoomSort, Status, UnreadThread, UnreadThreadPreview,
    IMAGE_THUMB_ROWS,
};

/// Minimum interval between background `/members` refreshes for a single room,
/// triggered by live messages from senders whose display name we don't yet know.
/// Collapses bursts of unknown senders into one fetch per room per window.
const MEMBERS_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);

/// Concurrent `/members` reads, matching the media pool's shape. The per-room
/// cooldown above bounds *repeats* of one room; this bounds how many rooms are
/// in flight at once, which is what a thousand-room list needs (#189).
pub(crate) const MEMBERS_WORKERS: usize = 4;
const UNREAD_THREAD_PREVIEW_LIMIT: usize = 3;

/// Result of a background `/members` refresh ([`App::spawn_members_refresh`]),
/// drained by the main loop and applied via [`App::apply_members_outcome`].
pub(crate) struct MembersOutcome {
    pub(super) room_key: RoomKey,
    /// `None` when the read failed. The outcome is still delivered so the
    /// caller can tell "no members to name this room after" (a permanent
    /// answer) from "the request did not land" (retry after the cooldown).
    pub(super) members: Option<Vec<MemberDto>>,
}

impl App {
    pub(crate) fn handle_live_frame(&mut self, frame: LiveFrame) -> LiveFrameAction {
        match frame {
            LiveFrame::Connected => {
                self.connection_state = ConnectionState::Connected;
                if !self.is_mid_command() {
                    self.status = Status::Debug("live WebSocket connected".to_owned());
                }
                // Read-on-reconnect: the lossy bus may have dropped frames while
                // we were down, so re-read any active flow's authoritative state
                // and discover a request that arrived while we were down
                // (ADR 0028 §3).
                self.resync_active_verification();
                self.discover_incoming_verification();
                // Typing overlays are live-only and the bus is lossy: a
                // "stopped typing" frame may have been missed while down, so
                // drop stale typing rather than leave a peer "typing" forever.
                self.clear_typing_overlays();
                LiveFrameAction::None
            }
            LiveFrame::Reconnecting { reason, delay } => {
                self.connection_state = ConnectionState::Reconnecting {
                    reason: reason.clone(),
                    delay,
                };
                self.clear_typing_overlays();
                if !self.is_mid_command() {
                    self.status = Status::Info(format!(
                        "live WebSocket reconnecting in {}s: {reason}",
                        delay.as_secs()
                    ));
                }
                LiveFrameAction::None
            }
            LiveFrame::Disconnected(reason) => {
                self.connection_state = ConnectionState::Disconnected(reason.clone());
                if !self.is_mid_command() {
                    self.status = Status::Debug(format!("live WebSocket disconnected: {reason}"));
                }
                LiveFrameAction::None
            }
            LiveFrame::ProtocolError(err) => {
                self.connection_state = ConnectionState::ProtocolError(err.clone());
                // Always surface parse failures — a malformed frame is a server/wire
                // bug that must not be silently hidden even during modal interactions.
                self.status = Status::Debug(format!("ignored malformed live frame: {err}"));
                LiveFrameAction::None
            }
            LiveFrame::Timeline(event) => self.append_live_event(*event),
            LiveFrame::Verification(frame) => self.handle_verification_frame(frame),
            LiveFrame::SenderTrustViolation {
                account_id,
                payload,
            } => self.handle_sender_trust_violation(account_id, payload),
            LiveFrame::DeviceState {
                account_id,
                payload,
            } => self.handle_device_state_frame(account_id, payload),
            LiveFrame::Ephemeral {
                account_id,
                payload,
            } => {
                self.handle_ephemeral_frame(account_id, payload, std::time::Instant::now());
                LiveFrameAction::None
            }
        }
    }

    /// Apply a live `verification.*` frame to the modal. A `requested` frame for
    /// an untracked flow auto-opens the modal (ADR 0028 §2); other frames update
    /// the flow on screen when they match it.
    fn handle_verification_frame(&mut self, frame: VerificationFrame) -> LiveFrameAction {
        let VerificationFrame {
            account_id,
            kind,
            payload,
        } = frame;
        match kind {
            VerificationFrameKind::Requested => {
                let tracked = self
                    .verification
                    .as_ref()
                    .is_some_and(|flow| flow.matches(account_id, &payload.flow_id));
                let pending_outgoing = self.verification.as_ref().is_some_and(|flow| {
                    flow.is_pending_outgoing_target(
                        account_id,
                        &payload.user_id,
                        payload.device_id.as_deref(),
                    )
                });
                if tracked {
                    if let Some(flow) = self.verification.as_mut() {
                        flow.apply_frame(kind, &payload);
                    }
                } else if pending_outgoing {
                    // The server may echo `verification.requested` before the
                    // start request returns its flow id. Treat it as our own
                    // in-flight request, not as unsolicited incoming work.
                } else if self.should_open_incoming_verification(account_id, &payload) {
                    self.open_incoming_verification(
                        account_id,
                        payload.flow_id.clone(),
                        payload.user_id.clone(),
                        payload.device_id.clone().unwrap_or_default(),
                    );
                }
            }
            _ => {
                // Accept the frame for the tracked flow — or adopt the flow_id of
                // an outgoing flow whose `POST …/verify` response is still in
                // flight (matched on account + device).
                let applies = self.verification.as_ref().is_some_and(|flow| {
                    flow.account_id == account_id
                        && (flow.flow_id.as_deref() == Some(payload.flow_id.as_str())
                            || flow.is_pending_outgoing_device(
                                account_id,
                                payload.device_id.as_deref(),
                            ))
                });
                if applies {
                    if let Some(flow) = self.verification.as_mut() {
                        flow.apply_frame(kind, &payload);
                    }
                    if kind == VerificationFrameKind::Done {
                        self.status = Status::from("verification complete".to_owned());
                        // The at-decrypt trust snapshots don't change live, but a
                        // refresh re-reads any rows re-decrypted post-verification.
                        return LiveFrameAction::RefreshRooms;
                    }
                }
            }
        }
        LiveFrameAction::None
    }

    /// Surface a `sender_trust.violation` overlay frame (ADR 0031): a visible
    /// alert plus a room refresh to re-read the affected sender's events.
    fn handle_sender_trust_violation(
        &mut self,
        _account_id: Uuid,
        payload: SenderTrustViolationDto,
    ) -> LiveFrameAction {
        if payload.verification_violation {
            self.status = Status::from(format!(
                "⚠ sender-trust violation: {} identity changed since it was verified",
                payload.user_id
            ));
        } else {
            self.status = Status::from(format!("sender-trust changed for {}", payload.user_id));
        }
        LiveFrameAction::RefreshRooms
    }

    fn append_live_event(&mut self, event: EventDto) -> LiveFrameAction {
        let key = RoomKey {
            account_id: event.account_id,
            room_id: event.room_id.clone(),
        };
        if let Some((target_id, new_body, new_content)) = event.edit_relation() {
            if let Some(events) = self.messages.events.get_mut(&key) {
                if let Some(target) = events.iter_mut().find(|item| item.event_id == target_id) {
                    target.body = Some(new_body.to_owned());
                    target.content = Some(new_content.clone());
                }
            }
            return LiveFrameAction::None;
        }
        // Field comparison, not `RoomKey::from(room) == key`: that allocated a
        // room-id `String` for every room in the list, for every live event
        // (#189).
        let known_room = self
            .rooms
            .rooms
            .iter()
            .any(|room| room.account_id == key.account_id && room.room_id == key.room_id);
        if known_room && self.is_own_membership_departure(&event, &key) {
            return LiveFrameAction::RefreshRooms;
        }
        if self
            .selected_room()
            .is_some_and(|room| RoomKey::from(room) == key)
        {
            let visible_before = self.selected_display_line_count();
            let old_scroll_bottom = visible_before.saturating_sub(self.messages.page_size);
            // Don't auto-scroll when the user has jumped to a historical date;
            // a live event arriving during history browse would otherwise kick
            // the view back to today.
            let should_follow_tail = self.last_jump_ts.is_none()
                && (self.messages.scroll == usize::MAX
                    || self.messages.scroll >= old_scroll_bottom);
            // Whether this event actually renders in the main timeline; a
            // hidden event (m.reaction, a state event filtered by
            // display.show_state_events) must not advance the read marker (see
            // the note_room_read call below), or it would clear a genuinely
            // unread message's badge cross-device.
            let event_shown = should_show_event(&event, &self.display);
            let should_select = self.messages.selection.is_none()
                && event_shown
                && thread_visible(
                    &event,
                    self.thread_panel.as_deref(),
                    &self.promoted_thread_events,
                );
            let event_id = event.event_id.clone();
            if self.live.pending_own_event_id.as_deref() == Some(&event_id) {
                self.live
                    .own_senders
                    .insert(event.account_id, event.sender.clone());
                self.live.pending_own_event_id = None;
            }
            self.remember_display_name_from_event(&key, &event);
            if self
                .messages
                .events
                .get(&key)
                .is_some_and(|events| events.iter().any(|e| e.event_id == event.event_id))
            {
                return LiveFrameAction::None;
            }
            // In historical view, don't append live events — they would
            // overwrite the jumped-to snapshot with today's messages.
            if self.last_jump_ts.is_some() {
                return LiveFrameAction::None;
            }
            let thread_root = event.thread_relation().map(str::to_owned);
            let should_mark_thread_unread = thread_root.as_deref().is_some_and(|_| {
                self.thread_event_counts_as_unread(&event, self.thread_panel.as_deref())
            });
            let account_id = event.account_id;
            let origin_ts = event.origin_ts;
            // Captured alongside `origin_ts` for the same reason: `event` is
            // moved into `self.messages.events` below, before the read is noted.
            let arrival_order = event.arrival_order;
            // A live message from a sender we have no name for (e.g. someone who
            // joined after the last full load) would render as a raw MXID until
            // the next room reload. Kick off a debounced /members refresh so it
            // resolves in place. Member events already seeded the map above.
            let sender_unknown = !self.display_name_known(&key, &event.sender);
            if let Some(root) = thread_root.as_deref() {
                self.apply_live_thread_member(&key, root);
                if should_mark_thread_unread {
                    self.mark_thread_unread_from_event(&key, root, &event);
                }
            }
            self.messages
                .events
                .entry(key.clone())
                .or_default()
                .push(event);
            if sender_unknown {
                self.spawn_members_refresh(key.clone());
            }
            if let Some(events) = self.messages.events.get_mut(&key) {
                events.sort_by_key(|event| event.origin_ts);
            }
            if let Some(root) = thread_root.as_deref() {
                // If the thread panel is not open for this root, promote the
                // new event so it appears in the main timeline. This ensures
                // the user sees new thread replies even when the panel is closed.
                if self.thread_panel.as_deref() != Some(root) {
                    self.promoted_thread_events.insert(event_id.clone());
                    self.spawn_live_thread_root_fetch(account_id, &key, root);
                }
            }
            if should_follow_tail {
                self.messages.scroll = usize::MAX;
            }
            // The room is on screen, so a *shown* live event counts as read —
            // the same rule that keeps its unread badge clear below (M12). A
            // hidden event (reaction, filtered state) advances nothing the user
            // can see, so it must not move the cross-device marker, and for the
            // same reason it is not a receipt target either (ADR 0089).
            if event_shown {
                self.note_room_read(
                    key.clone(),
                    super::read_markers::ReadMarker {
                        event_id: event_id.clone(),
                        origin_ts,
                    },
                    Some(super::read_markers::ReceiptTarget {
                        event_id: event_id.clone(),
                        arrival_order,
                    }),
                );
            }
            if should_select {
                self.messages.selection = Some(event_id);
            }
            self.rooms.unread.remove(&key);
            LiveFrameAction::None
        } else {
            if should_show_event(&event, &self.display) {
                if let Some(root) = event.thread_relation().map(str::to_owned) {
                    if self.thread_event_counts_as_unread(&event, None) {
                        self.mark_thread_unread_from_event(&key, &root, &event);
                    }
                }
                *self.rooms.unread.entry(key).or_default() += 1;
            }
            if known_room {
                LiveFrameAction::None
            } else {
                LiveFrameAction::RefreshRooms
            }
        }
    }

    fn is_own_membership_departure(&self, event: &EventDto, key: &RoomKey) -> bool {
        if !matches!(event.membership_change().as_deref(), Some("leave" | "ban")) {
            return false;
        }
        let Some(state_key) = event.state_key() else {
            return false;
        };
        self.rooms
            .rooms
            .iter()
            .find(|room| RoomKey::from(*room) == *key)
            .and_then(|room| room.account_user_id.as_deref())
            == Some(state_key)
    }

    pub(crate) fn is_own_event(&self, event: &EventDto) -> bool {
        self.live.own_senders.get(&event.account_id) == Some(&event.sender)
    }

    pub(crate) fn thread_event_counts_as_unread(
        &self,
        event: &EventDto,
        open_thread_root: Option<&str>,
    ) -> bool {
        should_show_event(event, &self.display)
            && open_thread_root != event.thread_relation()
            && !self.is_own_event(event)
    }

    pub(crate) fn mark_thread_unread_from_event(
        &mut self,
        key: &RoomKey,
        root: &str,
        event: &EventDto,
    ) {
        let sender = self.sender_label(event);
        let body = event.display_body();
        let entry = self
            .unread_threads
            .entry(key.clone())
            .or_default()
            .entry(root.to_owned())
            .or_insert_with(|| UnreadThread {
                root_event_id: root.to_owned(),
                unread_count: 0,
                latest_event_id: event.event_id.clone(),
                latest_sender: sender.clone(),
                latest_body: body.clone(),
                latest_ts: event.origin_ts,
                recent: Vec::new(),
                counted: std::collections::HashSet::new(),
            });
        // A reply can be observed twice — live, then again by a timeline load
        // while it is still past the read marker. Count each id once.
        if !entry.counted.insert(event.event_id.clone()) {
            return;
        }
        entry.unread_count = entry.unread_count.saturating_add(1);
        if event.origin_ts >= entry.latest_ts {
            entry.latest_event_id = event.event_id.clone();
            entry.latest_sender = sender.clone();
            entry.latest_body = body.clone();
            entry.latest_ts = event.origin_ts;
        }
        entry
            .recent
            .retain(|preview| preview.event_id != event.event_id);
        entry.recent.push(UnreadThreadPreview {
            event_id: event.event_id.clone(),
            sender,
            body,
            origin_ts: event.origin_ts,
        });
        entry.recent.sort_by(|a, b| {
            b.origin_ts
                .cmp(&a.origin_ts)
                .then_with(|| b.event_id.cmp(&a.event_id))
        });
        entry.recent.truncate(UNREAD_THREAD_PREVIEW_LIMIT);
    }

    pub(crate) fn rebuild_display_names(&mut self, room: &RoomDto, events: &[EventDto]) {
        let key = RoomKey::from(room);
        self.rooms.display_names.remove(&key);
        for event in events {
            self.remember_display_name_from_event(&key, event);
        }
    }

    /// Add display names from an incrementally loaded history slice without
    /// clearing or overriding names already known for the room. Full timeline
    /// loads rebuild from their complete snapshot and then seed current `/members`
    /// state; PageUp/PageDown loads only a partial slice, so replacing the map
    /// here would make existing senders fall back to raw MXIDs.
    pub(crate) fn merge_missing_display_names_from_events(
        &mut self,
        room: &RoomDto,
        events: &[EventDto],
    ) {
        let key = RoomKey::from(room);
        let map = self.rooms.display_names.entry(key).or_default();
        for event in events {
            if event.event_type != "m.room.member" {
                continue;
            }
            let user_id = event.state_key().unwrap_or(&event.sender);
            let Some(display_name) = event.membership_display_name() else {
                continue;
            };
            map.entry(user_id.to_owned())
                .or_insert_with(|| display_name.to_owned());
        }
    }

    /// Merge member-state display names into the map for `room`. Called after
    /// `rebuild_display_names` so the authoritative current state overwrites any
    /// stale name from an older membership event in the timeline page. Members
    /// with no `display_name` (or an empty one) are left as-is; we never blank
    /// out a name we already derived from the timeline.
    pub(crate) fn seed_display_names_from_members(
        &mut self,
        room: &RoomDto,
        members: &[MemberDto],
    ) {
        self.seed_display_names_for_key(RoomKey::from(room), members);
    }

    fn seed_display_names_for_key(&mut self, key: RoomKey, members: &[MemberDto]) {
        let map = self.rooms.display_names.entry(key).or_default();
        for member in members {
            if let Some(name) = member
                .display_name
                .as_deref()
                .filter(|n| !n.trim().is_empty())
            {
                map.insert(member.user_id.clone(), name.to_owned());
            }
        }
    }

    /// True when a non-empty display name is already known for `sender` in `key`'s
    /// room. Used to decide whether a live message warrants a `/members` refresh.
    fn display_name_known(&self, key: &RoomKey, sender: &str) -> bool {
        self.rooms
            .display_names
            .get(key)
            .and_then(|names| names.get(sender))
            .is_some_and(|name| !name.trim().is_empty())
    }

    /// Fetch `/members` in the background to resolve sender display names for a
    /// room the user is actively watching, triggered when a live message arrives
    /// from a sender we have no name for (e.g. someone who joined after the last
    /// full load). Rate-limited per room by [`MEMBERS_REFRESH_COOLDOWN`] so a
    /// burst of messages from unknown senders triggers at most one fetch per
    /// window; returns immediately and never blocks the event loop.
    pub(crate) fn spawn_members_refresh(&mut self, key: RoomKey) {
        let Some(tx) = self.members_tx.clone() else {
            return;
        };
        let now = Instant::now();
        if self
            .members_refresh_after
            .get(&key)
            .is_some_and(|after| now < *after)
        {
            return;
        }
        self.members_refresh_after
            .insert(key.clone(), now + MEMBERS_REFRESH_COOLDOWN);
        let client = self.client.clone();
        let account_id = key.account_id;
        let room_id = key.room_id.clone();
        let workers = self.members_workers.clone();
        tokio::spawn(async move {
            // Hold a permit for the request: without this the room-list title
            // sweep fans out one concurrent request per unnamed room.
            let Ok(_permit) = workers.acquire().await else {
                return;
            };
            let members = client.room_members(account_id, &room_id).await.ok();
            let _ = tx.send(MembersOutcome {
                room_key: key,
                members,
            });
        });
    }

    /// Apply a completed [`MembersOutcome`] by overlaying its display names onto
    /// the room map. Room-keyed, so a result that lands after the user navigates
    /// away updates that room's names harmlessly and shows when they return.
    pub(crate) fn apply_members_outcome(&mut self, outcome: MembersOutcome) {
        let MembersOutcome { room_key, members } = outcome;
        // A failed read says nothing about the room, so it must not be recorded
        // as "has no derivable title" — the cooldown allows a retry instead.
        let Some(members) = members else {
            return;
        };
        self.seed_display_names_for_key(room_key.clone(), &members);
        // For an unnamed room (e.g. a DM) derive a list title from its members so
        // it shows the other participant's name rather than the raw room id.
        let unnamed = self
            .rooms
            .rooms
            .iter()
            .find(|room| RoomKey::from(*room) == room_key)
            .filter(|room| {
                room.name.as_deref().is_none_or(|n| n.trim().is_empty())
                    && room
                        .canonical_alias
                        .as_deref()
                        .is_none_or(|a| a.trim().is_empty())
            });
        let derived = unnamed.and_then(|room| {
            super::dm_title_from_members(room.account_user_id.as_deref(), &members)
        });
        match derived {
            Some(title) => {
                self.rooms_without_derived_title.remove(&room_key);
                self.room_titles.insert(room_key, title);
                if matches!(self.room_sort, RoomSort::AlphaAsc | RoomSort::AlphaDesc) {
                    self.resort_rooms();
                }
            }
            // The read landed and there is nobody to name the room after, so
            // asking again changes nothing until its membership does. Record
            // that explicitly; the sweep used to re-request every cooldown for
            // the life of the process (#189).
            None if unnamed.is_some() => {
                self.rooms_without_derived_title.insert(room_key);
            }
            None => {}
        }
    }

    fn remember_display_name_from_event(&mut self, key: &RoomKey, event: &EventDto) {
        if event.event_type != "m.room.member" {
            return;
        }
        let user_id = event.state_key().unwrap_or(&event.sender);
        let Some(display_name) = event.membership_display_name() else {
            return;
        };
        self.rooms
            .display_names
            .entry(key.clone())
            .or_default()
            .insert(user_id.to_owned(), display_name.to_owned());
    }

    pub(crate) fn sender_label(&self, event: &EventDto) -> String {
        let key = RoomKey {
            account_id: event.account_id,
            room_id: event.room_id.clone(),
        };
        let display_name = self
            .rooms
            .display_names
            .get(&key)
            .and_then(|names| names.get(&event.sender))
            .filter(|name| !name.trim().is_empty())
            .cloned();
        if let Some(name) = display_name {
            return name;
        }
        // No displayname is known for this sender. Dense mode shortens the mxid
        // to its bare `@localpart` (dropping the homeserver); normal mode shows
        // the full `@user:homeserver`.
        match self.display.message_density {
            MessageDensity::Dense => super::account_localpart(&event.sender)
                .map(|local| format!("@{local}"))
                .unwrap_or_else(|| event.sender.clone()),
            MessageDensity::Normal => event.sender.clone(),
        }
    }

    pub(crate) fn selected_room(&self) -> Option<&RoomDto> {
        self.rooms
            .selected
            .and_then(|index| self.rooms.rooms.get(index))
    }

    pub(crate) fn selected_raw_events(&self) -> &[EventDto] {
        self.selected_room()
            .and_then(|room| self.messages.events.get(&RoomKey::from(room)))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub(crate) fn selected_reactions(&self) -> HashMap<String, Vec<(String, usize)>> {
        collect_reactions(self.selected_raw_events())
    }

    pub(crate) fn selected_events(&self) -> Vec<&EventDto> {
        let thread_panel = self.thread_panel.as_deref();
        self.selected_room()
            .and_then(|room| self.messages.events.get(&RoomKey::from(room)))
            .map(|events| {
                events
                    .iter()
                    .filter(|event| should_show_event(event, &self.display))
                    .filter(|event| {
                        thread_visible(event, thread_panel, &self.promoted_thread_events)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn selected_message_id(&self) -> Option<&str> {
        self.messages.selection.as_deref()
    }

    fn selection_status(&self, index: usize, count: usize) -> String {
        let mut s = format!("selected message {} of {}", index + 1, count);
        if self
            .selected_message_event()
            .and_then(|e| e.image_mxc())
            .is_some()
        {
            s.push_str(&format!(
                "  [{}: preview image]",
                self.shortcuts.media_preview.label()
            ));
        }
        // Hint the thread-open shortcut when the selected message heads a thread
        // and we are not already inside the panel (ADR 0032 M2).
        if self.thread_panel.is_none()
            && self
                .selected_message_event()
                .is_some_and(|event| self.is_thread_root(&event.event_id))
        {
            s.push_str(&format!(
                "  [{}: open thread]",
                self.shortcuts.thread.label()
            ));
        }
        s
    }

    pub(crate) fn selected_message_event(&self) -> Option<&EventDto> {
        let selected_message = self.messages.selection.as_deref()?;
        self.selected_events()
            .into_iter()
            .find(|event| event.event_id == selected_message)
    }

    pub(crate) fn move_selected_message(&mut self, offset: isize) {
        let Some((event_id, next, event_count)) = ({
            let events = self.selected_events();
            if events.is_empty() {
                None
            } else {
                let next = selected_message_target_index(
                    events.as_slice(),
                    self.messages.selection.as_deref(),
                    offset,
                );
                Some((events[next].event_id.clone(), next, events.len()))
            }
        }) else {
            self.messages.selection = None;
            self.status = Status::from("no displayed messages".to_owned());
            return;
        };
        self.messages.selection = Some(event_id);
        self.ensure_message_index_visible(next);
        self.status = Status::from(self.selection_status(next, event_count));
    }

    pub(crate) fn jump_to_first_message(&mut self) {
        let events = self.selected_events();
        if events.is_empty() {
            self.messages.selection = None;
            self.status = Status::from("no displayed messages".to_owned());
            return;
        }
        let count = events.len();
        let event_id = events[0].event_id.clone();
        self.messages.selection = Some(event_id);
        self.ensure_message_index_visible(0);
        self.status = Status::from(format!("selected message 1 of {}", count));
    }

    pub(crate) fn jump_to_last_message(&mut self) {
        self.last_jump_ts = None;
        let events = self.selected_events();
        if events.is_empty() {
            self.messages.selection = None;
            self.status = Status::from("no displayed messages".to_owned());
            return;
        }
        let count = events.len();
        let last = count - 1;
        let event_id = events[last].event_id.clone();
        self.messages.selection = Some(event_id);
        self.ensure_message_index_visible(last);
        self.status = Status::from(format!("selected message {} of {}", count, count));
    }

    pub(crate) fn page_selected_message(&mut self, direction: isize) {
        self.ensure_message_layout();
        let page = self.messages.page_size.max(1);
        let Some((event_id, next, event_count)) = ({
            let events = self.selected_events();
            if events.is_empty() {
                None
            } else {
                let ranges = self.cached_message_ranges();
                let total_lines = ranges
                    .last()
                    .map(|range| range.end)
                    .unwrap_or_default()
                    .max(1);
                let current_index = self
                    .messages
                    .selection
                    .as_deref()
                    .and_then(|event_id| events.iter().position(|event| event.event_id == event_id))
                    .unwrap_or_else(|| {
                        if direction.is_negative() {
                            message_index_at_line(
                                ranges,
                                self.messages.scroll.saturating_add(page.saturating_sub(1)),
                            )
                        } else {
                            message_index_at_line(ranges, self.messages.scroll)
                        }
                    });
                let current_line = ranges
                    .get(current_index)
                    .map(|range| range.start)
                    .unwrap_or_default();
                let target_line = if direction.is_negative() {
                    current_line.saturating_sub(page)
                } else {
                    current_line
                        .saturating_add(page)
                        .min(total_lines.saturating_sub(1))
                };
                let next = message_index_at_line(ranges, target_line);
                Some((events[next].event_id.clone(), next, events.len()))
            }
        }) else {
            self.messages.selection = None;
            self.status = Status::from("no displayed messages".to_owned());
            return;
        };
        self.messages.selection = Some(event_id);
        self.ensure_message_index_visible(next);
        self.status = Status::from(self.selection_status(next, event_count));
    }

    pub(crate) fn ensure_message_index_visible(&mut self, index: usize) {
        self.ensure_message_layout();
        let ranges = self.cached_message_ranges();
        let Some(range) = ranges.get(index) else {
            return;
        };
        let page_size = self.messages.page_size.max(1);
        let total_lines = ranges.last().map(|range| range.end).unwrap_or_default();
        let max_scroll = total_lines.saturating_sub(page_size);
        let mut scroll = self.messages.scroll.min(max_scroll);
        if range.start < scroll || range.end > scroll.saturating_add(page_size) {
            scroll = range.start;
        }
        self.messages.scroll = scroll.min(max_scroll);
    }

    /// Select the first loaded message at or after `pivot` (Unix ms) and center
    /// it in the pane, so a jump lands with later messages filling the lower
    /// half. Falls back to the newest loaded message when none is at or after
    /// `pivot` (e.g. the pivot lies past all loaded history).
    pub(crate) fn center_on_pivot(&mut self, pivot: i64) {
        let target = {
            let events = self.selected_events();
            events
                .iter()
                .position(|e| e.origin_ts >= pivot)
                .or_else(|| events.len().checked_sub(1))
                .map(|index| (index, events[index].event_id.clone()))
        };
        if let Some((index, event_id)) = target {
            self.messages.selection = Some(event_id);
            self.center_message_index(index);
        }
    }

    /// Scroll so the message at `index` sits roughly in the vertical middle of
    /// the pane, keeping earlier and later messages visible above and below it.
    /// Used by the day-skip shortcuts to frame the day they land on.
    pub(crate) fn center_message_index(&mut self, index: usize) {
        self.ensure_message_layout();
        let ranges = self.cached_message_ranges();
        let Some(range) = ranges.get(index) else {
            return;
        };
        let page_size = self.messages.page_size.max(1);
        let total_lines = ranges.last().map(|range| range.end).unwrap_or_default();
        let max_scroll = total_lines.saturating_sub(page_size);
        let scroll = range.start.saturating_sub(page_size / 2);
        self.messages.scroll = scroll.min(max_scroll);
    }

    pub(crate) async fn commit_room_search(&mut self, query: String) {
        if query.is_empty() {
            return;
        }
        let query_lower = query.to_ascii_lowercase();
        let all_matches: Vec<usize> = self
            .visible_room_indices()
            .into_iter()
            .filter(|&i| room_matches_search(&self.rooms.rooms[i], &query_lower))
            .collect();
        let found = all_matches.first().copied();
        self.last_search = Some(query);
        match found {
            Some(index) => {
                self.rooms.selected = Some(index);
                self.load_selected_timeline().await;
                self.status = match_status(1, all_matches.len());
            }
            None => self.status = Status::Info("no match".to_owned()),
        }
    }

    pub(crate) async fn search_adjacent_room(&mut self, query: &str, forward: bool) {
        let query = query.to_ascii_lowercase();
        let all_matches: Vec<usize> = self
            .visible_room_indices()
            .into_iter()
            .filter(|&i| room_matches_search(&self.rooms.rooms[i], &query))
            .collect();
        if all_matches.is_empty() {
            self.status = Status::Info("no more matches".to_owned());
            return;
        }
        let found = next_match_index(
            &all_matches,
            self.rooms.selected,
            forward,
            self.display.search_wrap,
        );
        match found {
            Some(index) => {
                self.rooms.selected = Some(index);
                self.load_selected_timeline().await;
                let match_num = all_matches.iter().position(|&i| i == index).unwrap_or(0) + 1;
                self.status = match_status(match_num, all_matches.len());
            }
            None => self.status = Status::Info("no more matches".to_owned()),
        }
    }

    pub(crate) fn commit_message_search(&mut self, query: String) {
        if query.is_empty() {
            return;
        }
        let query_lower = query.to_ascii_lowercase();
        let current_id = self.messages.selection.clone();
        let (found, total_matches) = {
            let events = self.selected_events();
            let all_matches: Vec<(usize, String)> = events
                .iter()
                .enumerate()
                .filter(|(_, event)| message_matches_search(event, &query_lower))
                .map(|(i, event)| (i, event.event_id.clone()))
                .collect();
            let total = all_matches.len();
            let cursor_pos = current_id
                .as_deref()
                .and_then(|id| events.iter().position(|e| e.event_id == id));
            let found = if let Some(pos) = cursor_pos {
                all_matches
                    .iter()
                    .find(|(i, _)| *i > pos)
                    .or_else(|| all_matches.first())
                    .cloned()
            } else {
                all_matches.first().cloned()
            };
            let match_num = found
                .as_ref()
                .and_then(|(i, _)| all_matches.iter().position(|(j, _)| j == i))
                .map(|p| p + 1)
                .unwrap_or(1);
            (found.map(|(i, id)| (i, id, match_num)), total)
        };
        self.last_search = Some(query);
        match found {
            Some((index, event_id, match_num)) => {
                self.messages.selection = Some(event_id);
                self.ensure_message_index_visible(index);
                self.status = match_status(match_num, total_matches);
            }
            None => self.status = Status::Info("no match".to_owned()),
        }
    }

    pub(crate) fn search_adjacent_message(&mut self, query: &str, forward: bool) {
        let query = query.to_ascii_lowercase();
        let current_id = self.messages.selection.clone();
        let (found, total_matches) = {
            let events = self.selected_events();
            let current_pos = current_id
                .as_deref()
                .and_then(|id| events.iter().position(|event| event.event_id == id));
            let all_matches: Vec<(usize, String)> = events
                .iter()
                .enumerate()
                .filter(|(_, event)| message_matches_search(event, &query))
                .map(|(i, event)| (i, event.event_id.clone()))
                .collect();
            let total = all_matches.len();
            let found = if forward {
                let start = current_pos.map(|i| i + 1).unwrap_or(0);
                let direct = all_matches.iter().find(|(i, _)| *i >= start).cloned();
                if direct.is_some() || !self.display.search_wrap {
                    direct
                } else {
                    all_matches.first().cloned()
                }
            } else {
                let end = current_pos.unwrap_or(events.len());
                let direct = all_matches.iter().rev().find(|(i, _)| *i < end).cloned();
                if direct.is_some() || !self.display.search_wrap {
                    direct
                } else {
                    all_matches.last().cloned()
                }
            };
            let match_num = found
                .as_ref()
                .and_then(|(i, _)| all_matches.iter().position(|(j, _)| j == i))
                .map(|p| p + 1);
            (found.map(|(i, id)| (i, id, match_num.unwrap_or(1))), total)
        };
        match found {
            Some((index, event_id, match_num)) => {
                self.messages.selection = Some(event_id);
                self.ensure_message_index_visible(index);
                self.status = match_status(match_num, total_matches);
            }
            None => self.status = Status::Info("no more matches".to_owned()),
        }
    }

    /// Total rendered lines for the selected timeline, from the cached layout.
    ///
    /// Read directly rather than through `ensure_message_layout`: the one
    /// caller measures the pane *before* appending a live event, so the layout
    /// from the last draw is precisely the pre-append state it wants.
    fn selected_display_line_count(&self) -> usize {
        self.cached_message_ranges()
            .last()
            .map(|range| range.end)
            .unwrap_or_default()
    }

    /// Per-image body heights, from the decoded-image cache.
    ///
    /// Shared by the layout cache and by `draw`, which previously derived this
    /// separately with the same logic. Only entries that differ from
    /// `IMAGE_THUMB_ROWS` are stored, so the map stays small: an empty map means
    /// every image is at the default height, while a missing single entry would
    /// force that image back to the default and desync nav from what is drawn.
    pub(crate) fn image_thumb_rows(&self, events: &[&EventDto]) -> ImageThumbRows {
        let font_size = self.picker.font_size();
        events
            .iter()
            .filter_map(|event| {
                let (account_id, mxc_url) = event.image_mxc()?;
                let key = MediaKey::new(account_id, mxc_url.clone());
                let thumb_h = if let Some(ImageState::Ready(img)) = self.image_cache.get(&key) {
                    let nat = Resize::natural_size(img, font_size);
                    (nat.height as usize).clamp(1, IMAGE_THUMB_ROWS)
                } else {
                    IMAGE_THUMB_ROWS
                };
                (thumb_h != IMAGE_THUMB_ROWS).then_some(((account_id, mxc_url), thumb_h))
            })
            .collect()
    }

    pub(crate) fn sender_labels(&self, events: &[&EventDto]) -> Vec<String> {
        events
            .iter()
            .map(|event| self.sender_label(event))
            .collect()
    }

    pub(crate) fn set_message_viewport(&mut self, page_size: usize, width: usize) {
        self.messages.page_size = page_size.max(1);
        self.messages.width = width.max(1);
    }
}

pub(crate) fn should_show_event(event: &EventDto, display: &DisplayOptions) -> bool {
    if event.event_type == "m.reaction" {
        return false;
    }
    display.show_state_events || event.is_message_event() || event.is_membership_event()
}

pub(crate) fn room_matches_search(room: &RoomDto, query: &str) -> bool {
    [
        Some(room.room_id.as_str()),
        room.canonical_alias.as_deref(),
        room.name.as_deref(),
        room.topic.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|field| contains_ascii_case_insensitive(field, query))
}

/// `haystack.to_ascii_lowercase().contains(needle)` without the allocation.
///
/// `needle` is already lowercased by the caller. The room-name filter runs this
/// over every room on every keystroke *and* every frame; allocating a lowercased
/// copy of each room's id, alias, name, and topic there was four `String`s per
/// room per pass (#189). Byte windows are exact here because
/// `to_ascii_lowercase` only ever folded ASCII, which is what
/// `eq_ignore_ascii_case` compares.
pub(crate) fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let needle = needle.as_bytes();
    haystack.len() >= needle.len()
        && haystack
            .as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

fn message_matches_search(event: &EventDto, query: &str) -> bool {
    if event.redacted {
        return false;
    }
    // Both callers pass an already-lowercased query, so this is the same match
    // the `to_ascii_lowercase().contains(..)` form made — without allocating a
    // lowercased copy of every message body on each `/n`, `/N` and search
    // commit, which is the allocation `room_matches_search` above already
    // stopped making.
    event
        .body
        .as_deref()
        .is_some_and(|body| contains_ascii_case_insensitive(body, query))
}

#[cfg(test)]
mod tests {
    use super::contains_ascii_case_insensitive;

    /// The allocation-free matcher must agree with the
    /// `to_ascii_lowercase().contains(..)` it replaced, including on the
    /// non-ASCII input where `to_ascii_lowercase` deliberately does nothing.
    #[test]
    fn case_insensitive_contains_matches_the_allocating_form() {
        let cases = [
            ("Ops Room", "ops"),
            ("Ops Room", "ROOM"),
            ("Ops Room", "room"),
            ("Ops Room", "zzz"),
            ("", "x"),
            ("x", ""),
            ("short", "much longer needle"),
            ("!AbC:example.com", "abc:example"),
            // `to_ascii_lowercase` leaves these alone, so both forms agree that
            // an uppercase non-ASCII needle does not match its lowercase form.
            ("Ärger", "ärger"),
            ("Ärger", "Ärger"),
            ("straße", "STRASSE"),
        ];
        for (haystack, needle) in cases {
            assert_eq!(
                contains_ascii_case_insensitive(haystack, needle),
                haystack
                    .to_ascii_lowercase()
                    .contains(&needle.to_ascii_lowercase()),
                "disagreed on {haystack:?} / {needle:?}"
            );
        }
    }
}
