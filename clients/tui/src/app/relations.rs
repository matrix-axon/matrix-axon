//! Reply and thread relation resolution for the message pane (ADR 0032).
//!
//! These helpers turn the raw `m.relates_to` blocks carried on each
//! [`EventDto`] into the rendered [`RelationContext`] the layout consumes: a
//! reply-context line for replying events and a `↳ N replies` badge for thread
//! roots. Resolution prefers the loaded slice, falling back to the cross-window
//! caches (`reply_targets`, `thread_summaries`) the API populates.

use uuid::Uuid;

use crate::api::{EventDto, RoomDto, ThreadSummaryDto};

use super::{
    App, Mode, RelationContext, ReplyPreview, RoomKey, Status, ThreadBadge, UnreadThreadEntry,
    UnreadThreadSelection, TIMELINE_LIMIT,
};

/// Maximum cross-window replied-to events fetched per room load, bounding the
/// fan-out of `GET …/events/{id}` calls against a reply-heavy room.
const REPLY_FETCH_CAP: usize = 20;

/// The result of a background relation refresh ([`App::spawn_relations_refresh`]),
/// drained by the main loop and applied via [`App::apply_relation_outcome`].
pub(crate) struct RelationOutcome {
    pub(super) room_key: RoomKey,
    pub(super) refresh_id: u64,
    pub(super) account_id: Uuid,
    /// `Some` installs the room's thread summaries (keyed by root id); `None`
    /// means the read failed and any stale summaries should be cleared.
    /// Ignored when `is_incremental` is true.
    pub(super) threads: Option<std::collections::HashMap<String, ThreadSummaryDto>>,
    /// Resolved replied-to events, keyed by their target event id.
    pub(super) replies: Vec<(String, EventDto)>,
    /// True for targeted single-event fetches triggered by live thread member
    /// promotion. Skips the refresh_id staleness check and leaves thread
    /// summaries untouched (avoids clearing a valid cache on a partial result).
    pub(super) is_incremental: bool,
}

impl App {
    pub(crate) fn open_unread_threads_picker(&mut self) {
        let entries = self.unread_thread_entries();
        if entries.is_empty() {
            self.unread_thread_selection = 0;
            self.unread_thread_selected = None;
            self.status = Status::from("no unread threads".to_owned());
            return;
        }
        self.sync_unread_thread_selection(&entries);
        self.popup_scroll = 0;
        self.mode = Mode::Popup(super::PopupKind::UnreadThreads);
        self.status = Status::Info(String::new());
    }

    pub(crate) fn move_unread_thread_selection(&mut self, delta: isize) {
        let entries = self.unread_thread_entries();
        let len = entries.len();
        if len == 0 {
            self.unread_thread_selection = 0;
            self.unread_thread_selected = None;
            return;
        }
        self.sync_unread_thread_selection(&entries);
        let current = self.unread_thread_selection.min(len.saturating_sub(1));
        let next = match delta.cmp(&0) {
            std::cmp::Ordering::Less => current.saturating_sub(delta.unsigned_abs()),
            std::cmp::Ordering::Equal => current,
            std::cmp::Ordering::Greater => current.saturating_add(delta as usize).min(len - 1),
        };
        self.set_unread_thread_selection(&entries, next);
    }

    pub(crate) async fn open_selected_unread_thread(&mut self) {
        let entries = self.unread_thread_entries();
        self.sync_unread_thread_selection(&entries);
        let Some(entry) = entries.get(self.unread_thread_selection).cloned() else {
            self.mode = Mode::Compose;
            self.unread_thread_selected = None;
            self.status = Status::from("no unread threads".to_owned());
            return;
        };
        let Some(room_index) = self
            .rooms
            .rooms
            .iter()
            .position(|room| RoomKey::from(room) == entry.room_key)
        else {
            self.mode = Mode::Compose;
            self.unread_thread_selected = None;
            self.status = Status::from("unread thread room is no longer available".to_owned());
            return;
        };
        let already_selected = self.rooms.selected == Some(room_index);
        self.mode = Mode::Compose;
        self.popup_scroll = 0;
        self.unread_thread_selection = 0;
        self.unread_thread_selected = None;
        self.prepare_room_selection(room_index);
        if !already_selected {
            self.load_selected_timeline().await;
        }
        self.open_thread_panel(entry.room_key.account_id, entry.root_event_id)
            .await;
    }

    /// Build the per-render relation context for the supplied displayed events.
    /// Reply previews are resolved for every replying event; thread badges are
    /// added to roots only in the main timeline (the panel labels its own root).
    /// Promoted thread member events also get a thread context preview pointing
    /// back to their root.
    pub(crate) fn relation_context(&self, events: &[&EventDto]) -> RelationContext {
        let mut ctx = RelationContext::default();
        for event in events {
            if let Some(target_id) = event.reply_relation() {
                if let Some(preview) = self.resolve_reply_preview(event.account_id, target_id) {
                    ctx.replies.insert(event.event_id.clone(), preview);
                }
            }
        }
        if let Some(root) = &self.thread_panel {
            ctx.thread_root = Some(root.clone());
            return ctx;
        }
        let summaries = self
            .selected_room()
            .map(RoomKey::from)
            .and_then(|key| self.thread_summaries.get(&key));
        for event in events {
            if let Some(badge) = self.thread_badge_for(event, summaries) {
                ctx.thread_badges.insert(event.event_id.clone(), badge);
            }
            // Promoted thread members: show thread context unless they also
            // have a reply_relation (that line already provides context).
            if event.reply_relation().is_none()
                && self.promoted_thread_events.contains(&event.event_id)
            {
                if let Some(root_id) = event.thread_relation() {
                    let preview = self.resolve_reply_preview(event.account_id, root_id);
                    ctx.thread_contexts.insert(event.event_id.clone(), preview);
                }
            }
        }
        ctx
    }

    pub(crate) fn sync_unread_thread_selection(&mut self, entries: &[UnreadThreadEntry]) {
        if entries.is_empty() {
            self.unread_thread_selection = 0;
            self.unread_thread_selected = None;
            return;
        }
        if let Some(selected) = &self.unread_thread_selected {
            if let Some(index) = entries.iter().position(|entry| {
                entry.room_key == selected.room_key && entry.root_event_id == selected.root_event_id
            }) {
                self.unread_thread_selection = index;
                return;
            }
        }
        let index = self
            .unread_thread_selection
            .min(entries.len().saturating_sub(1));
        self.set_unread_thread_selection(entries, index);
    }

    fn set_unread_thread_selection(&mut self, entries: &[UnreadThreadEntry], index: usize) {
        self.unread_thread_selection = index.min(entries.len().saturating_sub(1));
        self.unread_thread_selected = entries
            .get(self.unread_thread_selection)
            .map(UnreadThreadSelection::from);
    }

    /// Resolve the replied-to event for a reply-context line. Looks in the
    /// loaded slice first, then the cross-window `reply_targets` cache; `None`
    /// means the target is not yet resolved and the placeholder is shown.
    fn resolve_reply_preview(
        &self,
        account_id: uuid::Uuid,
        target_id: &str,
    ) -> Option<ReplyPreview> {
        let target = self
            .selected_raw_events()
            .iter()
            .find(|event| event.event_id == target_id)
            .or_else(|| self.reply_targets.get(&(account_id, target_id.to_owned())))?;
        Some(ReplyPreview {
            sender: self.sender_label(target),
            snippet: target.display_body(),
        })
    }

    /// Kick off a background refresh of a freshly loaded room's cross-window
    /// relation context (server thread summaries + replied-to events older than
    /// the slice). This returns immediately so the timeline paints right away;
    /// the fetched badges and reply context fill in a beat later when the
    /// [`RelationOutcome`] lands back on the main loop (ADR 0032 M3).
    ///
    /// All reads run **concurrently** as one batch, and the whole batch runs off
    /// the event loop, so a room load never blocks on relation fetches — the
    /// earlier inline version stalled switching into a reply-heavy room on up to
    /// `REPLY_FETCH_CAP + 1` HTTP calls before the timeline could redraw.
    pub(crate) fn spawn_relations_refresh(&mut self, room: &RoomDto) {
        let Some(tx) = self.relations_tx.clone() else {
            return;
        };
        let room_key = RoomKey::from(room);
        let refresh_id = self.relation_refresh_next_id;
        self.relation_refresh_next_id = self.relation_refresh_next_id.wrapping_add(1);
        self.relation_refresh_latest
            .insert(room_key.clone(), refresh_id);
        let account_id = room.account_id;
        let room_id = room.room_id.clone();
        let wanted = self.missing_reply_targets(room);
        let client = self.client.clone();
        tokio::spawn(async move {
            let threads = client.room_threads(account_id, &room_id);
            let replies = futures_util::future::join_all(wanted.into_iter().map(|target| {
                let client = client.clone();
                async move {
                    let result = client.get_event(account_id, &target).await;
                    (target, result)
                }
            }));
            let (threads, replies) = tokio::join!(threads, replies);
            let outcome = RelationOutcome {
                room_key,
                refresh_id,
                account_id,
                // `None` on error clears stale summaries (degrade to in-slice
                // counts); `Some` — even of an empty list — installs the result.
                threads: threads.ok().map(|list| {
                    list.into_iter()
                        .map(|summary| (summary.root_event_id.clone(), summary))
                        .collect()
                }),
                replies: replies
                    .into_iter()
                    .filter_map(|(target, result)| result.ok().map(|event| (target, event)))
                    .collect(),
                is_incremental: false,
            };
            let _ = tx.send(outcome);
        });
    }

    /// Apply a completed [`RelationOutcome`] to the relation caches. Results are
    /// room/account-keyed, so a result that arrives after the user has navigated
    /// away updates the cache harmlessly and surfaces if they return.
    pub(crate) fn apply_relation_outcome(&mut self, outcome: RelationOutcome) {
        if !outcome.is_incremental
            && self
                .relation_refresh_latest
                .get(&outcome.room_key)
                .is_some_and(|latest| *latest != outcome.refresh_id)
        {
            return;
        }
        if !outcome.is_incremental {
            match outcome.threads {
                Some(map) => {
                    self.thread_summaries.insert(outcome.room_key, map);
                }
                None => {
                    self.thread_summaries.remove(&outcome.room_key);
                }
            }
        }
        for (target, event) in outcome.replies {
            self.reply_targets
                .insert((outcome.account_id, target), event);
        }
    }

    /// Spawn a one-shot background fetch of a thread root event for promoted
    /// thread member rendering. Stores the result in `reply_targets` so
    /// `resolve_reply_preview` can find it on the next render frame. Skips the
    /// fetch when the root is already in the loaded slice or the cache, or when
    /// the relations channel has not been wired up (unit tests).
    pub(crate) fn spawn_live_thread_root_fetch(&self, account_id: Uuid, key: &RoomKey, root: &str) {
        let in_slice = self
            .messages
            .events
            .get(key)
            .is_some_and(|events| events.iter().any(|e| e.event_id == root));
        let in_cache = self
            .reply_targets
            .contains_key(&(account_id, root.to_owned()));
        if in_slice || in_cache {
            return;
        }
        let Some(tx) = self.relations_tx.clone() else {
            return;
        };
        let client = self.client.clone();
        let root_id = root.to_owned();
        let room_key = key.clone();
        tokio::spawn(async move {
            if let Ok(event) = client.get_event(account_id, &root_id).await {
                let outcome = RelationOutcome {
                    room_key,
                    refresh_id: 0,
                    account_id,
                    threads: None,
                    replies: vec![(root_id, event)],
                    is_incremental: true,
                };
                let _ = tx.send(outcome);
            }
        });
    }

    /// Keep a cached server count current after a live thread member arrives.
    /// The live bus sends the raw member event, not a refreshed aggregate row.
    pub(crate) fn apply_live_thread_member(&mut self, key: &RoomKey, root: &str) {
        if let Some(summary) = self
            .thread_summaries
            .get_mut(key)
            .and_then(|summaries| summaries.get_mut(root))
        {
            summary.reply_count = summary.reply_count.saturating_add(1);
        }
    }

    /// The replied-to event ids referenced by the loaded slice that are neither
    /// in the slice nor already cached, deduplicated and capped. These are the
    /// events the reply-context line needs but the timeline window doesn't carry.
    fn missing_reply_targets(&self, room: &RoomDto) -> Vec<String> {
        let key = RoomKey::from(room);
        let mut wanted: Vec<String> = Vec::new();
        let Some(events) = self.messages.events.get(&key) else {
            return wanted;
        };
        let in_slice: std::collections::HashSet<&str> =
            events.iter().map(|event| event.event_id.as_str()).collect();
        for event in events {
            if let Some(target) = event.reply_relation() {
                if !in_slice.contains(target)
                    && !self
                        .reply_targets
                        .contains_key(&(room.account_id, target.to_owned()))
                    && !wanted.iter().any(|id| id == target)
                {
                    wanted.push(target.to_owned());
                }
            }
        }
        wanted.truncate(REPLY_FETCH_CAP);
        wanted
    }

    /// Whether `event_id` heads a thread: it has in-slice members or a
    /// server-aggregated summary with replies.
    pub(super) fn is_thread_root(&self, event_id: &str) -> bool {
        let has_members = self
            .selected_raw_events()
            .iter()
            .any(|event| event.thread_relation() == Some(event_id));
        let has_summary = self
            .selected_room()
            .map(RoomKey::from)
            .and_then(|key| self.thread_summaries.get(&key))
            .and_then(|map| map.get(event_id))
            .is_some_and(|summary| summary.reply_count > 0);
        has_members || has_summary
    }

    /// Open the thread panel anchored at `root`: page in the full thread timeline
    /// and the root event (best effort), then switch the message pane into panel
    /// mode (ADR 0032 M2/M3).
    pub(crate) async fn open_thread_panel(&mut self, account_id: Uuid, root: String) {
        let Some(room) = self.selected_room().cloned() else {
            return;
        };
        let key = RoomKey::from(&room);
        self.clear_unread_thread(&key, &root);
        if let Ok(mut page) = self
            .client
            .thread_timeline(account_id, &room.room_id, &root, None, TIMELINE_LIMIT)
            .await
        {
            page.events.reverse();
            self.merge_room_events(&key, page.events);
        }
        let have_root = self
            .messages
            .events
            .get(&key)
            .is_some_and(|events| events.iter().any(|event| event.event_id == root));
        if !have_root {
            if let Ok(event) = self.client.get_event(account_id, &root).await {
                self.merge_room_events(&key, vec![event]);
            }
        }
        // Clear any promoted events that belong to this thread — they are now
        // visible inside the panel itself and no longer need main-timeline promotion.
        let panel_members: Vec<String> = self
            .messages
            .events
            .get(&key)
            .map(|events| {
                events
                    .iter()
                    .filter(|e| e.thread_relation() == Some(root.as_str()))
                    .map(|e| e.event_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        for id in panel_members {
            self.promoted_thread_events.remove(&id);
        }
        self.thread_panel = Some(root);
        self.force_terminal_clear = true;
        // Return focus to the input box so the user can immediately type a reply
        // into the thread — especially when the panel was opened by the hotkey
        // from message-list navigation (ADR 0032 M2).
        self.mode = Mode::Compose;
        // Anchor the view to the newest message in the thread using the panel's
        // own ranges. Setting `scroll = usize::MAX` alone is not enough: the
        // viewport math runs against freshly-built ranges, so without an explicit
        // `ensure_message_index_visible` the first frame can land on a stale
        // offset and render blank until the user navigates (ADR 0032 M2).
        let count = self.selected_events().len();
        if count > 0 {
            self.ensure_message_index_visible(count - 1);
        } else {
            self.messages.scroll = usize::MAX;
        }
        self.messages.selection = None;
        self.status = Status::Info(format!(
            "[in thread] {} reply(ies) — Esc to exit",
            count.saturating_sub(1)
        ));
    }

    pub(crate) fn clear_unread_thread(&mut self, key: &RoomKey, root: &str) {
        let clear_room = if let Some(threads) = self.unread_threads.get_mut(key) {
            threads.remove(root);
            threads.is_empty()
        } else {
            false
        };
        if clear_room {
            self.unread_threads.remove(key);
        }
    }

    /// Close the thread panel and return to the main timeline. Returns `true`
    /// when a panel was open (so the caller can treat `Esc` as consumed).
    pub(crate) fn close_thread_panel(&mut self) -> bool {
        if let Some(root) = self.thread_panel.take() {
            // thread_panel is now None, so selected_events() returns the main timeline
            let index = self
                .selected_events()
                .iter()
                .position(|e| e.event_id == root);
            self.messages.selection = Some(root);
            self.force_terminal_clear = true;
            if let Some(idx) = index {
                self.ensure_message_index_visible(idx);
            } else {
                self.messages.scroll = usize::MAX;
            }
            self.status = Status::from("left thread".to_owned());
            true
        } else {
            false
        }
    }

    /// Merge fetched events into a room's loaded slice, deduped by event id and
    /// kept in chronological order. Thread members merged this way are hidden
    /// from the main timeline by [`thread_visible`] and surfaced in the panel.
    fn merge_room_events(&mut self, key: &RoomKey, incoming: Vec<EventDto>) {
        let slice = self.messages.events.entry(key.clone()).or_default();
        let mut changed = false;
        for event in incoming {
            if !slice
                .iter()
                .any(|existing| existing.event_id == event.event_id)
            {
                slice.push(event);
                changed = true;
            }
        }
        if changed {
            slice.sort_by_key(|event| event.origin_ts);
        }
    }

    /// Build a thread badge for `event` when it is a thread root: it has in-slice
    /// members and/or a server-aggregated summary. The count prefers the server
    /// total; the latest-reply excerpt is resolved from in-slice members.
    fn thread_badge_for(
        &self,
        event: &EventDto,
        summaries: Option<&std::collections::HashMap<String, ThreadSummaryDto>>,
    ) -> Option<ThreadBadge> {
        let members: Vec<&EventDto> = self
            .selected_raw_events()
            .iter()
            .filter(|candidate| candidate.thread_relation() == Some(event.event_id.as_str()))
            .collect();
        let server_count = summaries
            .and_then(|map| map.get(&event.event_id))
            .map(|summary| summary.reply_count)
            .filter(|count| *count > 0);
        if members.is_empty() && server_count.is_none() {
            return None;
        }
        let count = server_count.unwrap_or(members.len() as i64);
        let latest = members.iter().max_by_key(|member| member.origin_ts);
        let unread_count = self
            .selected_room()
            .map(RoomKey::from)
            .and_then(|key| self.unread_threads.get(&key))
            .and_then(|threads| threads.get(&event.event_id))
            .map(|thread| thread.unread_count)
            .unwrap_or_default();
        Some(ThreadBadge {
            count,
            unread_count,
            latest_sender: latest.map(|member| self.sender_label(member)),
            latest_snippet: latest.map(|member| member.display_body()),
        })
    }

    pub(crate) fn unread_thread_entries(&self) -> Vec<UnreadThreadEntry> {
        let mut entries = Vec::new();
        for (room_key, threads) in &self.unread_threads {
            let Some(room) = self
                .rooms
                .rooms
                .iter()
                .find(|room| RoomKey::from(*room) == *room_key)
            else {
                continue;
            };
            for thread in threads.values() {
                let root_snippet = self
                    .messages
                    .events
                    .get(room_key)
                    .and_then(|events| {
                        events
                            .iter()
                            .find(|event| event.event_id == thread.root_event_id)
                    })
                    .or_else(|| {
                        self.reply_targets
                            .get(&(room_key.account_id, thread.root_event_id.clone()))
                    })
                    .map(|event| event.display_body());
                entries.push(UnreadThreadEntry {
                    room_key: room_key.clone(),
                    room_title: self.room_list_title(room),
                    root_event_id: thread.root_event_id.clone(),
                    root_snippet,
                    unread_count: thread.unread_count,
                    latest_sender: thread.latest_sender.clone(),
                    latest_body: thread.latest_body.clone(),
                    latest_ts: thread.latest_ts,
                    recent: thread.recent.clone(),
                });
            }
        }
        entries.sort_by(|a, b| {
            b.latest_ts
                .cmp(&a.latest_ts)
                .then_with(|| a.room_title.cmp(&b.room_title))
                .then_with(|| a.root_event_id.cmp(&b.root_event_id))
        });
        entries
    }
}

/// Whether an event is visible given the active thread-panel state (ADR 0032
/// M2). In the panel, only the root and its members show; in the main timeline,
/// thread members are normally hidden (their discussion lives behind the root's
/// badge). Exception: promoted live thread member events are always shown in
/// the main timeline so the user sees new replies even when the panel is closed.
/// This is a display-only filter — thread members still flow through
/// `should_show_event` for unread counting.
pub(crate) fn thread_visible(
    event: &EventDto,
    thread_panel: Option<&str>,
    promoted: &std::collections::HashSet<String>,
) -> bool {
    match thread_panel {
        Some(root) => event.event_id == root || event.thread_relation() == Some(root),
        None => event.thread_relation().is_none() || promoted.contains(&event.event_id),
    }
}
