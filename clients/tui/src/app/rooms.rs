use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::api::{ApiError, AxonClient, EventDto, MemberDto, RoomDto, TimelinePage};
use crate::config::TuiConfig;

use super::{
    display_body_with_sender, format_time, match_status, next_match_index, relative_room_index,
    AccountSelection, App, RoomKey, RoomSort, RoomTargetResolution, Status, TIMELINE_LIMIT,
};

/// Rows either side of the visible room-list window that still get their titles
/// fetched, so a short scroll does not stall waiting on a request.
const ROOM_TITLE_LOOKAHEAD: usize = 8;

/// Window height assumed before the first draw has measured the panel.
const ROOM_TITLE_DEFAULT_PAGE: usize = 32;

impl App {
    pub(crate) fn apply_room_refresh(&mut self, mut rooms: Vec<RoomDto>) {
        // A logged-out (deactivated) account keeps its rows in Axon's `events`
        // table, and `GET /v1/rooms` joins accounts without a state filter, so it
        // still lists that account's rooms. Drop rooms for any account we know is
        // not active so a logout actually clears them. Rooms for accounts we don't
        // know about are kept, so a stale or failed account fetch never blanks the
        // whole list.
        rooms.retain(|room| !self.is_known_inactive_account(room.account_id));
        sort_rooms_by_pin_with_title(&mut rooms, &self.pinned_rooms, self.room_sort, |room| {
            room_list_title_from_cache(&self.room_titles, room)
        });
        let selected_key = self.selected_room().map(RoomKey::from);
        let previous_keys: Vec<RoomKey> = self.rooms.rooms.iter().map(RoomKey::from).collect();
        let refreshed_keys: HashSet<RoomKey> = rooms.iter().map(RoomKey::from).collect();
        self.rooms.rooms = rooms;
        for key in previous_keys {
            if !refreshed_keys.contains(&key) {
                self.prune_room_caches(&key);
            }
        }
        self.rooms.selected = selected_key
            .and_then(|key| {
                self.rooms
                    .rooms
                    .iter()
                    .position(|room| RoomKey::from(room) == key)
            })
            .or_else(|| {
                self.rooms
                    .selected
                    .filter(|index| *index < self.rooms.rooms.len())
            });
        self.seed_own_senders_from_rooms();
        if self.rooms.rooms.is_empty() {
            self.rooms.selected = None;
            if !self.is_mid_command() {
                self.status = Status::from("no rooms returned by Axon".to_owned());
            }
        } else if self
            .rooms
            .selected
            .is_none_or(|selected| !self.visible_room_indices().contains(&selected))
        {
            let visible = self.visible_room_indices();
            self.rooms.selected = visible.first().copied();
            if !self.is_mid_command() {
                self.status = Status::from(format!("loaded {} rooms", self.rooms.rooms.len()));
            }
        } else if !self.is_mid_command() {
            self.status = Status::from(format!("refreshed {} rooms", self.rooms.rooms.len()));
        }
        self.sweep_visible_room_titles();
    }

    fn prune_room_caches(&mut self, key: &RoomKey) {
        self.rooms.display_names.remove(key);
        self.rooms.unread.remove(key);
        self.room_titles.remove(key);
        self.messages.events.remove(key);
        self.messages.history_cursors.remove(key);
        self.thread_summaries.remove(key);
        self.relation_refresh_latest.remove(key);
        self.members_refresh_after.remove(key);
        self.rooms_without_derived_title.remove(key);
        self.drafts_written_since_fetch.remove(key);
        self.unread_threads.remove(key);
    }

    /// Kick off background `/members` reads for the rooms **on screen** that have
    /// no `m.room.name`/alias and no cached member-derived title yet (typically
    /// DMs), so the room list can show the other participant's name.
    ///
    /// Demand-driven, matching the rule media already follows: request what the
    /// user can see plus a small lookahead, and let scrolling pull in the rest.
    /// This used to sweep the *entire* list on every refresh, which on a server
    /// with thousands of unnamed rooms fanned out thousands of concurrent
    /// requests, each of whose results ran an O(n) scan and (under alpha sort) a
    /// full re-sort — the room-count-squared behaviour behind #189.
    ///
    /// Called after every room refresh and from the main loop's tick, so a
    /// scroll or a filter change pulls in the newly visible rooms.
    pub(crate) fn sweep_visible_room_titles(&mut self) {
        let visible = self.visible_room_indices();
        if visible.is_empty() {
            return;
        }
        // `rooms.page_size` is zero until the first draw; the loop paints before
        // the room list lands now, but a default keeps this correct either way.
        let page = if self.rooms.page_size == 0 {
            ROOM_TITLE_DEFAULT_PAGE
        } else {
            self.rooms.page_size
        };
        let start = self.rooms.scroll.saturating_sub(ROOM_TITLE_LOOKAHEAD);
        let end = self
            .rooms
            .scroll
            .saturating_add(page)
            .saturating_add(ROOM_TITLE_LOOKAHEAD)
            .min(visible.len());
        let keys: Vec<RoomKey> = visible[start.min(end)..end]
            .iter()
            .filter_map(|index| self.rooms.rooms.get(*index))
            .filter(|room| is_likely_dm(room))
            .map(RoomKey::from)
            .filter(|key| {
                !self.room_titles.contains_key(key)
                    && !self.rooms_without_derived_title.contains(key)
            })
            .collect();
        for key in keys {
            self.spawn_members_refresh(key);
        }
    }

    /// Whether `key` is currently pinned. Used by the renderer to draw the
    /// pinned/unpinned separator (ADR 0038).
    pub(crate) fn is_room_pinned(&self, key: &RoomKey) -> bool {
        self.pinned_rooms.contains(key)
    }

    /// Re-sort the loaded rooms in place after a pin/unpin or sort-mode change,
    /// keeping the same room selected.
    pub(super) fn resort_rooms(&mut self) {
        let selected_key = self.selected_room().map(RoomKey::from);
        sort_rooms_by_pin_with_title(
            &mut self.rooms.rooms,
            &self.pinned_rooms,
            self.room_sort,
            |room| room_list_title_from_cache(&self.room_titles, room),
        );
        if let Some(key) = selected_key {
            self.rooms.selected = self
                .rooms
                .rooms
                .iter()
                .position(|room| RoomKey::from(room) == key);
        }
    }

    /// Resolve the room a `/pin`/`/unpin` request targets: the explicit argument
    /// if given, otherwise the currently selected room. Sets a status message and
    /// returns `None` when resolution fails.
    fn resolve_pin_room_index(&mut self, target: Option<&str>) -> Option<usize> {
        match target {
            Some(target) => match self.resolve_room_target(target) {
                RoomTargetResolution::Match(index) => Some(index),
                RoomTargetResolution::Ambiguous(options) => {
                    self.status =
                        Status::Info(format!("room name is ambiguous: {}", options.join(", ")));
                    None
                }
                RoomTargetResolution::Missing => {
                    self.status = Status::from(format!("room not found: {target}"));
                    None
                }
            },
            None => {
                let index = self.rooms.selected;
                if index.is_none() {
                    self.status = Status::from("select a room to pin".to_owned());
                }
                index
            }
        }
    }

    /// Pin the target room (or re-pin an already-pinned room to the top of the
    /// pinned section). Writes the new state to the config file immediately.
    pub(crate) fn pin_room(&mut self, target: Option<&str>) {
        let Some(index) = self.resolve_pin_room_index(target) else {
            return;
        };
        let key = RoomKey::from(&self.rooms.rooms[index]);
        let title = self.rooms.rooms[index].title().to_owned();
        self.pinned_rooms.retain(|existing| existing != &key);
        self.pinned_rooms.insert(0, key);
        self.resort_rooms();
        self.status = match self.persist_pinned_rooms() {
            Ok(()) => Status::from(format!("pinned {title}")),
            Err(err) => Status::from(format!("pinned {title} (config save failed: {err})")),
        };
    }

    /// Unpin the target room. No-op (with a status message) if it is not pinned.
    pub(crate) fn unpin_room(&mut self, target: Option<&str>) {
        let Some(index) = self.resolve_pin_room_index(target) else {
            return;
        };
        let key = RoomKey::from(&self.rooms.rooms[index]);
        let title = self.rooms.rooms[index].title().to_owned();
        if !self.pinned_rooms.iter().any(|existing| existing == &key) {
            self.status = Status::from(format!("{title} is not pinned"));
            return;
        }
        self.pinned_rooms.retain(|existing| existing != &key);
        self.resort_rooms();
        self.status = match self.persist_pinned_rooms() {
            Ok(()) => Status::from(format!("unpinned {title}")),
            Err(err) => Status::from(format!("unpinned {title} (config save failed: {err})")),
        };
    }

    fn persist_pinned_rooms(&self) -> Result<(), String> {
        let entries: Vec<String> = self
            .pinned_rooms
            .iter()
            .map(RoomKey::to_config_entry)
            .collect();
        TuiConfig::save_pinned_rooms(&self.config_path, &entries).map_err(|err| err.to_string())
    }

    /// Whether `account_id` is an account we've listed and that is *not* active
    /// (e.g. logged out). Unknown accounts return `false` so we never hide a
    /// room just because our account list is empty or stale.
    fn is_known_inactive_account(&self, account_id: Uuid) -> bool {
        self.accounts.inactive_ids.contains(&account_id)
    }

    pub(crate) fn seed_own_senders_from_rooms(&mut self) {
        self.live
            .own_senders
            .extend(self.rooms.rooms.iter().filter_map(|room| {
                room.account_user_id
                    .as_ref()
                    .map(|user_id| (room.account_id, user_id.clone()))
            }));
    }

    /// Rebuild a room's sender display-name map from a freshly loaded timeline
    /// page, then overlay the authoritative `/members` state. Every load that
    /// *replaces* the page (full load, `/jump`) must go through this: a partial
    /// page rarely carries an `m.room.member` event for every sender, so without
    /// the `/members` overlay most senders would regress to raw MXIDs. A failed
    /// `/members` read leaves the page-derived names in place.
    pub(crate) async fn reseed_display_names(&mut self, room: &RoomDto, events: &[EventDto]) {
        self.rebuild_display_names(room, events);
        if let Ok(members) = self
            .client
            .room_members(room.account_id, &room.room_id)
            .await
        {
            self.seed_display_names_from_members(room, &members);
        }
    }

    pub(crate) async fn load_selected_timeline(&mut self) {
        let Some(room) = self.selected_room().cloned() else {
            return;
        };
        self.messages.selection = None;
        self.messages.scroll = usize::MAX;
        self.last_jump_ts = None;
        self.force_terminal_clear = true;
        match self
            .client
            .room_timeline(room.account_id, &room.room_id, None, None, TIMELINE_LIMIT)
            .await
        {
            Ok(mut page) => {
                page.events.reverse();
                apply_edits(&mut page.events);
                let has_more = page.next_cursor.is_some();
                let key = RoomKey::from(&room);
                match page.next_cursor {
                    Some(c) => {
                        self.messages.history_cursors.insert(key.clone(), c);
                    }
                    None => {
                        self.messages.history_cursors.remove(&key);
                    }
                }
                self.reseed_display_names(&room, &page.events).await;
                // Thread replies newer than the read marker made this room
                // unread while hidden behind their roots' badges — promote
                // them so what caused the badge is visible (M12). Collected
                // against the *pre-advance* marker, so this must precede
                // note_room_read; the root fetches run after the page is
                // installed so in-slice roots aren't re-fetched.
                let unseen_thread_roots = self.collect_unseen_thread_promotions(&key, &page.events);
                // Opening the room reads it up to its newest loaded event (M12).
                // Two positions, two orders, one candidate set: the marker names
                // the display-last event (`page.events` is ascending by
                // `origin_ts`), the receipt the greatest `arrival_order` among
                // the same displayed events (ADR 0089). The marker used to read
                // the raw page here, so a trailing hidden state event advanced
                // it past everything rendered (#167).
                if let Some(targets) = super::read_markers::read_targets_for(
                    &page.events,
                    &self.display,
                    &self.promoted_thread_events,
                ) {
                    self.note_room_read(key.clone(), targets.marker, Some(targets.receipt));
                }
                self.messages.events.insert(key.clone(), page.events);
                for (account_id, root) in unseen_thread_roots {
                    self.spawn_live_thread_root_fetch(account_id, &key, &root);
                }
                self.rooms.unread.remove(&key);
                self.thread_panel = None;
                self.spawn_relations_refresh(&room);
                if !self.is_mid_command() {
                    self.status = Status::Info(if has_more {
                        format!("showing {} (older history available later)", room.title())
                    } else {
                        format!("showing {}", room.title())
                    });
                }
            }
            Err(err) => {
                if !self.is_mid_command() {
                    self.status = Status::from(format!("timeline load failed: {err}"));
                }
            }
        }
        // Entering a room swaps the compose buffer to that room's draft (M12),
        // settling the previous room's pending draft first so a switch can't
        // drop it or misattribute it.
        self.sync_draft_on_room_change();
    }

    /// Fetch the next older page of history for the current room and prepend it
    /// to the in-memory event list. Triggered by PageUp / Up-arrow at the top.
    pub(crate) async fn load_more_history(&mut self) {
        let Some(room) = self.selected_room().cloned() else {
            return;
        };
        let key = RoomKey::from(&room);

        let Some(cursor) = self.messages.history_cursors.get(&key).cloned() else {
            self.status = Status::Info("at beginning of Axon history".to_owned());
            return;
        };
        if self.messages.loading_history {
            return;
        }

        const MAX_EVENTS: usize = 500;
        let current_count = self.messages.events.get(&key).map(|v| v.len()).unwrap_or(0);
        if current_count >= MAX_EVENTS {
            self.status = Status::Info(format!(
                "showing {MAX_EVENTS} messages — End to return to live"
            ));
            return;
        }

        self.messages.loading_history = true;
        self.status = Status::Info("Loading older messages…".to_owned());

        match self
            .client
            .room_timeline(
                room.account_id,
                &room.room_id,
                Some(&cursor),
                None,
                TIMELINE_LIMIT,
            )
            .await
        {
            Ok(mut page) => {
                // Server returns newest-first; reverse so index 0 is oldest.
                page.events.reverse();
                let new_cursor = page.next_cursor.clone();
                let loaded = page.events.len();

                // Prepend older events using a swap to avoid O(n*k) repeated inserts.
                let mut new_events = page.events;
                let spare = MAX_EVENTS.saturating_sub(current_count);
                keep_adjacent_older_tail(&mut new_events, spare);
                let prepended = new_events.len();
                let existing = self.messages.events.entry(key.clone()).or_default();
                new_events.append(existing);
                *existing = new_events;

                // Run apply_edits on the full combined slice so cross-page edits
                // (an edit on the newer page targeting an original on the older
                // page we just loaded) are resolved correctly.
                apply_edits(existing);

                // Anchor the viewport to the previously selected event so the
                // user's reading position stays stable after the prepend.
                let anchor_id = self.messages.selection.clone();
                if let Some(id) = &anchor_id {
                    let events = self.selected_events();
                    if let Some(new_index) = events.iter().position(|e| &e.event_id == id) {
                        self.ensure_message_index_visible(new_index);
                    }
                } else {
                    // Nothing selected: position at the first event of the new page.
                    self.messages.selection = self
                        .messages
                        .events
                        .get(&key)
                        .and_then(|v| v.first())
                        .map(|e| e.event_id.clone());
                    self.ensure_message_index_visible(0);
                }

                // Clone the newly-prepended slice for display-name building before
                // the mutable borrow for cursor insertion below.
                let new_slice: Vec<EventDto> = self
                    .messages
                    .events
                    .get(&key)
                    .map(|v| v[..prepended].to_vec())
                    .unwrap_or_default();
                self.merge_missing_display_names_from_events(&room, &new_slice);

                match new_cursor {
                    Some(c) => {
                        self.messages.history_cursors.insert(key.clone(), c);
                    }
                    None => {
                        self.messages.history_cursors.remove(&key);
                    }
                }

                let has_more = self.messages.history_cursors.contains_key(&key);
                let capped = prepended < loaded;
                self.status = Status::Info(if capped {
                    format!("loaded {prepended} older messages (capped at {MAX_EVENTS} total)")
                } else if has_more {
                    format!("loaded {prepended} older messages — PageUp for more")
                } else {
                    format!("loaded {prepended} older messages — beginning of Axon history")
                });
            }
            Err(err) => {
                self.status = Status::Info(format!("history load failed: {err}"));
            }
        }

        self.messages.loading_history = false;
    }

    /// Navigate the current room's timeline so the moment `ts` (Unix ms) is
    /// centered in the message pane, with later messages filling the lower half
    /// of the screen rather than the target being pinned to the bottom edge.
    /// Replaces the in-memory event list entirely.
    ///
    /// The timeline read is end-anchored (newest events at or before `at_ts`),
    /// so we expand a window forward from `ts` until the returned page holds
    /// enough messages after `ts` to fill the lower half — while it still
    /// straddles `ts` so earlier context survives in the same page. Iteration is
    /// capped so a server that does not honor `at_ts` cannot spin forever.
    ///
    /// Returns `true` when a non-empty page was loaded.
    pub(crate) async fn jump_to_date(&mut self, ts: i64) -> bool {
        let Some(room) = self.selected_room().cloned() else {
            self.status = Status::Info("select a room before using /jump".to_owned());
            return false;
        };
        let date_label = format_jump_date(ts);
        self.status = Status::Info(format!("Jumping to {date_label}…"));
        // Fill the lower half of the pane with messages after `ts`.
        let want_after = self.messages.page_size / 2;
        let Some(mut page) = self.fetch_straddling_page(&room, ts, want_after).await else {
            self.status = Status::Info(format!("no messages found near {date_label}"));
            return false;
        };
        page.events.reverse();
        apply_edits(&mut page.events);
        let key = RoomKey::from(&room);
        match page.next_cursor {
            Some(c) => {
                self.messages.history_cursors.insert(key.clone(), c);
            }
            None => {
                self.messages.history_cursors.remove(&key);
            }
        }
        self.reseed_display_names(&room, &page.events).await;
        let earliest_ts = page.events.first().map(|e| e.origin_ts);
        self.messages.events.insert(key, page.events);
        self.messages.selection = None;
        self.last_jump_ts = Some(ts);
        self.messages.scroll = 0;
        self.thread_panel = None;
        // Center the first message at or after `ts` so later messages fill below.
        self.center_on_pivot(ts);
        self.status = Status::Info(if earliest_ts.is_some_and(|e| e > ts) {
            format!("{date_label} is before the earliest messages — showing oldest available")
        } else {
            format!("Jumped to {date_label} — PageUp for older, End for newest")
        });
        true
    }

    /// Jump to the earliest message the Axon server has for the current room.
    /// The timeline API only pages backward (older) via `next_cursor`, so we walk
    /// back from the newest page, discarding intermediate pages, until one reports
    /// no older history — that page holds the start. A page cap guards against a
    /// runaway on a very long room; if hit, the oldest *loaded* page is shown and
    /// PageUp can continue from there.
    pub(crate) async fn jump_to_top(&mut self) {
        let Some(room) = self.selected_room().cloned() else {
            self.status = Status::Info("select a room before using /top".to_owned());
            return;
        };
        if self.messages.loading_history {
            return;
        }
        self.messages.loading_history = true;
        self.status = Status::Info(format!("Jumping to the start of {}…", room.title()));

        const MAX_PAGES: usize = 200;
        let mut cursor: Option<String> = None;
        let mut earliest: Option<crate::api::TimelinePage> = None;
        for _ in 0..MAX_PAGES {
            let page = match self
                .client
                .room_timeline(
                    room.account_id,
                    &room.room_id,
                    cursor.as_deref(),
                    None,
                    TIMELINE_LIMIT,
                )
                .await
            {
                Ok(page) => page,
                Err(err) => {
                    self.messages.loading_history = false;
                    self.status = Status::from(format!("/top failed: {err}"));
                    return;
                }
            };
            let next = page.next_cursor.clone();
            earliest = Some(page);
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        self.messages.loading_history = false;

        let Some(mut page) = earliest else {
            self.status = Status::Info(format!("no messages found in {}", room.title()));
            return;
        };
        if page.events.is_empty() {
            self.status = Status::Info(format!("no messages found in {}", room.title()));
            return;
        }
        let reached_start = page.next_cursor.is_none();
        page.events.reverse();
        apply_edits(&mut page.events);
        let key = RoomKey::from(&room);
        match page.next_cursor.take() {
            Some(c) => {
                self.messages.history_cursors.insert(key.clone(), c);
            }
            None => {
                self.messages.history_cursors.remove(&key);
            }
        }
        self.reseed_display_names(&room, &page.events).await;
        self.messages.events.insert(key, page.events);
        self.messages.selection = None;
        // Mark a historical view so live frames stop appending and PageDown pages
        // forward; pivot 0 keeps us anchored before all history.
        self.last_jump_ts = Some(0);
        self.messages.scroll = 0;
        self.thread_panel = None;
        self.spawn_relations_refresh(&room);
        self.status = Status::Info(if reached_start {
            format!(
                "Showing the earliest messages in {} — PageDown for newer, End for latest",
                room.title()
            )
        } else {
            format!(
                "Showing the oldest loaded messages in {} — PageUp for older, End for latest",
                room.title()
            )
        });
    }

    /// Fetch a single timeline page (newest-first, as the API returns it) that
    /// straddles `pivot` (Unix ms): it carries `pivot`'s neighbourhood plus up to
    /// `want_after` messages newer than `pivot`, while still reaching back to
    /// `pivot` so earlier context survives in the same page.
    ///
    /// The timeline read is end-anchored (newest events at or before `at_ts`), so
    /// we expand a window forward from `pivot` until the page holds enough later
    /// messages, while keeping it straddling `pivot`. `want_after` is clamped to
    /// half a page so there is always room for earlier context. Iteration is
    /// capped so a server that does not honor `at_ts` cannot spin forever.
    pub(crate) async fn fetch_straddling_page(
        &self,
        room: &RoomDto,
        pivot: i64,
        want_after: usize,
    ) -> Option<crate::api::TimelinePage> {
        fetch_straddling_timeline_page(
            self.client.clone(),
            room.account_id,
            room.room_id.clone(),
            pivot,
            TIMELINE_LIMIT,
            want_after,
        )
        .await
        .ok()
    }

    /// Fetch the next *newer* page of history and append it to the in-memory
    /// event list — the forward counterpart to [`load_more_history`]. Only acts
    /// in a jumped (historical) view; in the live view the tail already follows
    /// new events. Triggered by Down / PageDown at the bottom of a jumped buffer.
    /// When no newer messages remain, clears the jump anchor so live updates
    /// resume and the view follows the tail again.
    pub(crate) async fn load_newer_history(&mut self) {
        // In the live view the buffer already ends at the tail and live frames
        // append automatically; forward paging only makes sense after a jump.
        if self.last_jump_ts.is_none() || self.messages.loading_history {
            return;
        }
        let Some(room) = self.selected_room().cloned() else {
            return;
        };
        let key = RoomKey::from(&room);
        let Some(newest_ts) = self
            .messages
            .events
            .get(&key)
            .and_then(|v| v.last())
            .map(|e| e.origin_ts)
        else {
            return;
        };

        const MAX_EVENTS: usize = 500;
        let current_count = self.messages.events.get(&key).map(|v| v.len()).unwrap_or(0);
        if current_count >= MAX_EVENTS {
            self.status = Status::Info(format!(
                "showing {MAX_EVENTS} messages — End to return to live"
            ));
            return;
        }

        self.messages.loading_history = true;
        self.status = Status::Info("Loading newer messages…".to_owned());

        // A full page of forward progress, while still straddling `newest_ts` so
        // the appended run is contiguous with what we already have.
        let mut newer: Vec<EventDto> = match self
            .fetch_straddling_page(&room, newest_ts, TIMELINE_LIMIT / 2)
            .await
        {
            Some(mut page) => {
                page.events.reverse();
                page.events
                    .into_iter()
                    .filter(|e| e.origin_ts > newest_ts)
                    .collect()
            }
            None => Vec::new(),
        };
        if let Some(existing) = self.messages.events.get(&key) {
            let have: std::collections::HashSet<&str> =
                existing.iter().map(|e| e.event_id.as_str()).collect();
            newer.retain(|e| !have.contains(e.event_id.as_str()));
        }

        if newer.is_empty() {
            // Caught up to the present: drop the jump anchor so live frames append
            // again, and settle the view on the newest loaded message.
            self.last_jump_ts = None;
            let events = self.selected_events();
            if let Some(last) = events.last() {
                let index = events.len() - 1;
                self.messages.selection = Some(last.event_id.clone());
                self.ensure_message_index_visible(index);
            }
            self.status = Status::Info("at the latest messages — live updates resumed".to_owned());
        } else {
            let spare = MAX_EVENTS.saturating_sub(current_count);
            newer.truncate(spare);
            let new_slice = newer.clone();
            let first_new_id = newer.first().map(|e| e.event_id.clone());
            if let Some(existing) = self.messages.events.get_mut(&key) {
                existing.append(&mut newer);
                apply_edits(existing);
            }
            self.merge_missing_display_names_from_events(&room, &new_slice);
            // Advance the selection into the freshly loaded run so the downward
            // scroll continues from where the user was.
            if let Some(id) = first_new_id {
                let events = self.selected_events();
                if let Some(index) = events.iter().position(|e| e.event_id == id) {
                    self.messages.selection = Some(id);
                    self.ensure_message_index_visible(index);
                }
            }
            self.status = Status::Info(
                "loaded newer messages — PageDown for more, End for latest".to_owned(),
            );
        }

        self.messages.loading_history = false;
    }

    /// Origin timestamp of the newest message at or before `ts` (Unix ms), or
    /// `None` if there is none. Used by the backward day-skip to discover which
    /// earlier day actually has messages before centering on it.
    pub(crate) async fn last_event_ts_at_or_before(&self, ts: i64) -> Option<i64> {
        let room = self.selected_room()?.clone();
        let page = self
            .client
            .room_timeline(room.account_id, &room.room_id, None, Some(ts), 1)
            .await
            .ok()?;
        page.events.first().map(|e| e.origin_ts)
    }

    /// Find the origin timestamp of the earliest message at or after `lower`
    /// (Unix ms), or `None` if no message exists at or after that point. Used by
    /// the forward day-skip to locate the next day that actually has messages.
    ///
    /// The timeline read is end-anchored (newest events at or before `at_ts`),
    /// so we first expand a window forward until it contains content at or after
    /// `lower`, then page backward to pin the *earliest* such message when the
    /// window is denser than one page. Iteration is capped so a server that does
    /// not honor `at_ts` cannot spin this loop forever.
    pub(crate) async fn find_first_event_at_or_after(&self, lower: i64) -> Option<i64> {
        const DAY_MS: i64 = 86_400_000;
        let room = self.selected_room()?.clone();
        let now = chrono::Utc::now().timestamp_millis();
        let mut earliest: Option<i64> = None;
        let mut window = DAY_MS;
        // Once we are paging backward toward `lower`, `probe` holds the next
        // `at_ts`; while `None` we are still expanding the forward window.
        let mut probe: Option<i64> = None;

        for _ in 0..64 {
            let at_ts = match probe {
                Some(p) => p,
                None => lower.saturating_add(window).saturating_sub(1).min(now),
            };
            let page = self
                .client
                .room_timeline(
                    room.account_id,
                    &room.room_id,
                    None,
                    Some(at_ts),
                    TIMELINE_LIMIT,
                )
                .await
                .ok()?;
            // The API returns events newest-first.
            let newest = page.events.first().map(|e| e.origin_ts);
            let oldest = page.events.last().map(|e| e.origin_ts);
            if let Some(pe) = page
                .events
                .iter()
                .rev()
                .map(|e| e.origin_ts)
                .find(|ts| *ts >= lower)
            {
                earliest = Some(earliest.map_or(pe, |e| e.min(pe)));
            }
            let full = page.events.len() == TIMELINE_LIMIT;

            match (newest, oldest) {
                // No events at or before `at_ts`: expand the window unless we
                // have already reached the present (or are paging backward).
                (None, _) | (Some(_), None) => {
                    if probe.is_some() || at_ts >= now {
                        return earliest;
                    }
                    window = window.saturating_mul(2);
                }
                // Window holds only messages older than `lower`: extend it
                // forward to reach into the gap's far side.
                (Some(n), Some(_)) if probe.is_none() && n < lower => {
                    if at_ts >= now {
                        return earliest;
                    }
                    window = window.saturating_mul(2);
                }
                // The page reaches back to `lower` (or is a short final page):
                // the earliest event we have seen is the true earliest.
                (Some(_), Some(o)) if !full || o <= lower => return earliest,
                // The page is full and entirely after `lower`; the real earliest
                // is older, so page backward toward `lower`.
                (Some(_), Some(o)) => probe = Some(o - 1),
            }
        }
        earliest
    }

    pub(super) async fn switch_room(&mut self, target: &str) {
        let index = match self.resolve_room_target(target) {
            RoomTargetResolution::Match(index) => index,
            RoomTargetResolution::Ambiguous(options) => {
                self.status =
                    Status::Info(format!("room name is ambiguous: {}", options.join(", ")));
                return;
            }
            RoomTargetResolution::Missing => {
                self.status = Status::from(format!("room not found: {target}"));
                return;
            }
        };
        self.rooms.selected = Some(index);
        self.load_selected_timeline().await;
    }

    pub(crate) async fn switch_relative_room(&mut self, offset: isize) {
        let visible = self.visible_room_indices();
        if visible.is_empty() {
            self.status = Status::from("no rooms to switch".to_owned());
            return;
        }
        let current_vis = self
            .rooms
            .selected
            .and_then(|sel| visible.iter().position(|&i| i == sel))
            .unwrap_or(0);
        let next_vis = relative_room_index(current_vis, visible.len(), offset);
        self.rooms.selected = Some(visible[next_vis]);
        self.load_selected_timeline().await;
    }

    pub(crate) fn sync_room_selection_to_account_filter(&mut self) {
        let visible = self.visible_room_indices();
        let current_ok = self
            .rooms
            .selected
            .is_some_and(|sel| visible.contains(&sel));
        if !current_ok {
            self.rooms.selected = visible.first().copied();
            self.messages.selection = None;
            self.messages.scroll = usize::MAX;
        }
    }

    pub(crate) fn cycle_account(&mut self, offset: isize) {
        let n = self.accounts.accounts.len();
        if n == 0 {
            return;
        }
        let total = n + 1;
        let current = match self.accounts.selected {
            AccountSelection::All => 0,
            AccountSelection::Account(i) => i + 1,
        };
        let next = ((current as isize + offset).rem_euclid(total as isize)) as usize;
        self.accounts.selected = if next == 0 {
            AccountSelection::All
        } else {
            AccountSelection::Account(next - 1)
        };
        self.sync_room_selection_to_account_filter();
    }

    pub(crate) fn search_adjacent_account(&mut self, query: &str, forward: bool) {
        let q = query.to_lowercase();
        let all_matches = self.account_search_matches(&q);
        if all_matches.is_empty() {
            self.status = Status::from("no more matches".to_owned());
            return;
        }
        let current_pos = match self.accounts.selected {
            AccountSelection::All => 0,
            AccountSelection::Account(i) => i + 1,
        };
        let found = next_match_index(
            &all_matches,
            Some(current_pos),
            forward,
            self.display.search_wrap,
        );
        if let Some(pos) = found {
            self.accounts.selected = if pos == 0 {
                AccountSelection::All
            } else {
                AccountSelection::Account(pos - 1)
            };
            self.sync_room_selection_to_account_filter();
            self.last_search = Some(query.to_owned());
            let match_num = all_matches.iter().position(|&p| p == pos).unwrap_or(0) + 1;
            self.status = match_status(match_num, all_matches.len());
        }
    }

    pub(crate) fn commit_account_search(&mut self, query: String) -> bool {
        let query_lower = query.to_lowercase();
        let all_matches = self.account_search_matches(&query_lower);
        self.last_search = Some(query.clone());
        let Some(&pos) = all_matches.first() else {
            self.status = Status::from(format!("no account matches: {query}"));
            return false;
        };
        self.accounts.selected = if pos == 0 {
            AccountSelection::All
        } else {
            AccountSelection::Account(pos - 1)
        };
        self.sync_room_selection_to_account_filter();
        self.status = match_status(1, all_matches.len());
        true
    }

    fn account_search_matches(&self, query_lower: &str) -> Vec<usize> {
        std::iter::once(AccountSelection::All.display_label(None))
            .chain(
                self.accounts
                    .accounts
                    .iter()
                    .enumerate()
                    .map(|(i, a)| AccountSelection::Account(i).display_label(Some(&a.user_id))),
            )
            .enumerate()
            .filter(|(_, label)| label.to_lowercase().contains(query_lower))
            .map(|(pos, _)| pos)
            .collect()
    }

    pub(super) fn switch_account(&mut self, target: &str) -> bool {
        let target = target.trim();

        if target.eq_ignore_ascii_case("all") || target == "0" {
            self.accounts.selected = AccountSelection::All;
            self.sync_room_selection_to_account_filter();
            self.status = Status::from("showing all accounts".to_owned());
            return true;
        }

        if let Ok(n) = target.parse::<usize>() {
            return match n
                .checked_sub(1)
                .filter(|&i| i < self.accounts.accounts.len())
            {
                Some(idx) => {
                    let user_id = self.accounts.accounts[idx].user_id.clone();
                    self.accounts.selected = AccountSelection::Account(idx);
                    self.sync_room_selection_to_account_filter();
                    self.status = Status::from(format!("account: {user_id}"));
                    true
                }
                None => {
                    self.status = Status::from(format!("account index out of range: {target}"));
                    false
                }
            };
        }

        let target_lower = target.to_lowercase();
        let exact: Vec<usize> = self
            .accounts
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.user_id.to_lowercase() == target_lower)
            .map(|(i, _)| i)
            .collect();
        if let Some(idx) = single_match(exact) {
            let user_id = self.accounts.accounts[idx].user_id.clone();
            self.accounts.selected = AccountSelection::Account(idx);
            self.sync_room_selection_to_account_filter();
            self.status = Status::from(format!("account: {user_id}"));
            return true;
        }

        let localpart = target.trim_start_matches('@');
        let local_matches: Vec<usize> = self
            .accounts
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| account_localpart(&a.user_id) == Some(localpart))
            .map(|(i, _)| i)
            .collect();
        if let Some(result) = resolve_account_matches(self, local_matches) {
            return result.apply(self);
        }

        let prefix_matches: Vec<usize> = self
            .accounts
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.user_id.to_lowercase().contains(&target_lower))
            .map(|(i, _)| i)
            .collect();
        if let Some(result) = resolve_account_matches(self, prefix_matches) {
            return result.apply(self);
        }

        self.status = Status::from(format!("account not found: {target}"));
        false
    }

    pub(super) async fn show_event(&mut self, event_id: &str) {
        let Some(room) = self.selected_room() else {
            self.status = Status::from("select a room before using /event".to_owned());
            return;
        };
        match self.client.get_event(room.account_id, event_id).await {
            Ok(event) => {
                let sender = self.sender_label(&event);
                let relation = if event.relates_to.is_some() {
                    " related"
                } else {
                    ""
                };
                let redaction = event
                    .redaction_event_id
                    .as_deref()
                    .map(|id| format!(" redacted_by={id}"))
                    .unwrap_or_default();
                self.status = Status::Info(format!(
                    "{} {} {} {}{}{}",
                    format_time(event.origin_ts, self.display.time_format),
                    sender,
                    event.event_id,
                    display_body_with_sender(&event, &sender)
                        .chars()
                        .take(120)
                        .collect::<String>(),
                    relation,
                    redaction
                ));
            }
            Err(err) => self.status = Status::Info(format!("event read failed: {err}")),
        }
    }
}

pub(super) async fn fetch_straddling_timeline_page(
    client: AxonClient,
    account_id: Uuid,
    room_id: String,
    pivot: i64,
    limit: usize,
    want_after: usize,
) -> Result<TimelinePage, ApiError> {
    const DAY_MS: i64 = 86_400_000;
    let now = chrono::Utc::now().timestamp_millis();
    let want_after = want_after.clamp(1, (limit / 2).max(1));

    let mut chosen: Option<TimelinePage> = None;
    let mut window = DAY_MS;
    for _ in 0..32 {
        let at_ts = pivot.saturating_add(window).min(now);
        let page = client
            .room_timeline(account_id, &room_id, None, Some(at_ts), limit)
            .await?;
        if page.events.is_empty() {
            if at_ts >= now {
                break;
            }
            window = window.saturating_mul(2);
            continue;
        }

        let oldest = page
            .events
            .last()
            .map(|event| event.origin_ts)
            .unwrap_or(pivot);
        let after_count = page
            .events
            .iter()
            .filter(|event| event.origin_ts > pivot)
            .count();
        let full = page.events.len() == limit;
        let has_earlier = oldest <= pivot;

        if !has_earlier && full && chosen.is_some() {
            break;
        }
        chosen = Some(page);
        if after_count >= want_after || at_ts >= now || (!has_earlier && full) {
            break;
        }
        window = window.saturating_mul(2);
    }

    Ok(chosen.unwrap_or(TimelinePage {
        events: Vec::new(),
        next_cursor: None,
    }))
}

/// Whether a room is *likely* a DM (ADR 0042). Interim heuristic: a room with no
/// `name` and no `canonical_alias` is treated as an unnamed/direct room. This is
/// imperfect (a named two-person room reads as a group, an unnamed small group
/// reads as a DM) and is slated to be replaced by the server-derived `is_direct`
/// from ADR 0043 / PR #174 — swap the body here when that lands.
pub(crate) fn is_likely_dm(room: &RoomDto) -> bool {
    room.name.as_deref().is_none_or(|n| n.trim().is_empty())
        && room
            .canonical_alias
            .as_deref()
            .is_none_or(|a| a.trim().is_empty())
}

/// Order rooms with pinned rooms first (by their position in `pinned`, most
/// recently pinned first — ADR 0038), then unpinned rooms by the active
/// [`RoomSort`] (ADR 0042). The pinned section keeps its pin order regardless of
/// the sort mode, since distinct pin ranks never reach the tiebreak.
pub(crate) fn sort_rooms_by_pin_with_title<F>(
    rooms: &mut [RoomDto],
    pinned: &[RoomKey],
    sort: RoomSort,
    title: F,
) where
    F: Fn(&RoomDto) -> String,
{
    // Both halves of the key used to be recomputed inside the comparator: the
    // pin rank was a linear scan of `pinned`, and the alpha tiebreak allocated
    // two lowercased `String`s — so O(n log n) scans and allocations for a
    // sort that runs on every room refresh (#189). Index the pins once, and let
    // `sort_by_cached_key` compute each room's key exactly once.
    //
    // Borrowing `pinned` (not `rooms`) keeps this free of the `&mut rooms`
    // borrow, and the tuple key avoids building a `RoomKey` per lookup.
    let pin_rank: HashMap<(Uuid, &str), usize> = pinned
        .iter()
        .enumerate()
        .map(|(index, key)| ((key.account_id, key.room_id.as_str()), index))
        .collect();
    // Lower rank (earlier in `pinned`) sorts first; unpinned rooms get
    // usize::MAX and fall to the bottom. Ties use the active sort mode.
    let rank = |room: &RoomDto| {
        pin_rank
            .get(&(room.account_id, room.room_id.as_str()))
            .copied()
            .unwrap_or(usize::MAX)
    };
    match sort {
        RoomSort::RecentActivity => {
            rooms.sort_by_cached_key(|room| (rank(room), Reverse(room.last_activity_ts)))
        }
        RoomSort::OldestActivity => {
            rooms.sort_by_cached_key(|room| (rank(room), room.last_activity_ts))
        }
        RoomSort::AlphaAsc => {
            rooms.sort_by_cached_key(|room| (rank(room), title(room).to_lowercase()))
        }
        RoomSort::AlphaDesc => {
            rooms.sort_by_cached_key(|room| (rank(room), Reverse(title(room).to_lowercase())))
        }
    }
}

#[cfg(test)]
fn sort_rooms_by_pin(rooms: &mut [RoomDto], pinned: &[RoomKey], sort: RoomSort) {
    sort_rooms_by_pin_with_title(rooms, pinned, sort, |room| room.title().to_owned());
}

fn keep_adjacent_older_tail<T>(events: &mut Vec<T>, spare: usize) {
    if events.len() > spare {
        let drop_count = events.len() - spare;
        events.drain(..drop_count);
    }
}

fn room_list_title_from_cache(room_titles: &HashMap<RoomKey, String>, room: &RoomDto) -> String {
    let named = room.name.as_deref().is_some_and(|n| !n.trim().is_empty())
        || room
            .canonical_alias
            .as_deref()
            .is_some_and(|a| !a.trim().is_empty());
    if named {
        return room.title().to_owned();
    }
    room_titles
        .get(&RoomKey::from(room))
        .cloned()
        .unwrap_or_else(|| room.title().to_owned())
}

fn single_match(indices: Vec<usize>) -> Option<usize> {
    match indices.as_slice() {
        [idx] => Some(*idx),
        _ => None,
    }
}

enum AccountResolution {
    Match(usize),
    Ambiguous(Vec<String>),
}

impl AccountResolution {
    fn apply(self, app: &mut App) -> bool {
        match self {
            AccountResolution::Match(idx) => {
                let user_id = app.accounts.accounts[idx].user_id.clone();
                app.accounts.selected = AccountSelection::Account(idx);
                app.sync_room_selection_to_account_filter();
                app.status = Status::from(format!("account: {user_id}"));
                true
            }
            AccountResolution::Ambiguous(options) => {
                app.status = Status::from(format!("account is ambiguous: {}", options.join(", ")));
                false
            }
        }
    }
}

fn resolve_account_matches(app: &App, indices: Vec<usize>) -> Option<AccountResolution> {
    match indices.as_slice() {
        [] => None,
        [idx] => Some(AccountResolution::Match(*idx)),
        _ => Some(AccountResolution::Ambiguous(
            indices
                .iter()
                .map(|&i| app.accounts.accounts[i].user_id.clone())
                .collect(),
        )),
    }
}

/// Format a Unix-millisecond timestamp as a human-readable date for status messages.
fn format_jump_date(ts_ms: i64) -> String {
    let secs = ts_ms / 1000;
    let days = secs / 86400;
    // Days since 1970-01-01 → calendar date (Gregorian civil calendar algorithm)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

pub(crate) fn account_localpart(user_id: &str) -> Option<&str> {
    user_id
        .strip_prefix('@')?
        .split_once(':')
        .map(|(local, _)| local)
}

/// Derive a display title for an unnamed room (e.g. a DM) from its member list,
/// excluding the account's own user (`self_user`). Uses each other member's
/// display name, falling back to their `@localpart`. Lists up to three names and
/// appends `, +N` for the rest. Returns `None` when there is no other member
/// (e.g. a note-to-self room), so the caller keeps the existing fallback.
pub(crate) fn dm_title_from_members(
    self_user: Option<&str>,
    members: &[MemberDto],
) -> Option<String> {
    let mut others: Vec<&MemberDto> = members
        .iter()
        .filter(|member| Some(member.user_id.as_str()) != self_user)
        .collect();
    if others.is_empty() {
        return None;
    }
    // Stable ordering so the title doesn't reshuffle between fetches.
    others.sort_by(|a, b| a.user_id.cmp(&b.user_id));
    let names: Vec<String> = others.iter().take(3).map(|m| member_display(m)).collect();
    let mut title = names.join(", ");
    if others.len() > 3 {
        title.push_str(&format!(", +{}", others.len() - 3));
    }
    Some(title)
}

/// A member's display name, falling back to `@localpart`, then the raw user id.
fn member_display(member: &MemberDto) -> String {
    member
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            account_localpart(&member.user_id)
                .map(|local| format!("@{local}"))
                .unwrap_or_else(|| member.user_id.clone())
        })
}

pub(crate) fn apply_edits(events: &mut Vec<EventDto>) {
    // Collect all edit relations, tracking whether each target is present on
    // this page. An edit whose target lives on an older (not-yet-loaded) page
    // must NOT be removed: suppressing it would make the edit invisible.
    let edits: Vec<(String, String, serde_json::Value)> = events
        .iter()
        .filter_map(|event| {
            let (target, body, content) = event.edit_relation()?;
            Some((target.to_owned(), body.to_owned(), content.clone()))
        })
        .collect();
    let mut applied: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (target_id, new_body, new_content) in &edits {
        if let Some(event) = events.iter_mut().find(|event| &event.event_id == target_id) {
            event.body = Some(new_body.clone());
            event.content = Some(new_content.clone());
            applied.insert(target_id.clone());
        }
    }
    // Only collapse an edit event once its target has been updated on this page.
    events.retain(|event| {
        event
            .edit_relation()
            .is_none_or(|(target, _, _)| !applied.contains(target))
    });
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use crate::api::EventDto;

    use crate::api::RoomDto;
    use crate::app::{RoomKey, RoomSort};

    use super::{
        apply_edits, is_likely_dm, keep_adjacent_older_tail, sort_rooms_by_pin,
        sort_rooms_by_pin_with_title,
    };

    /// The title sweep asks only for the rooms on screen (plus a lookahead),
    /// not the whole list.
    ///
    /// The old sweep walked every unnamed room on every refresh, so a
    /// thousand-room list fanned out a thousand concurrent `/members` reads
    /// whose results each ran an O(n) scan and, under alpha sort, a full
    /// re-sort (#189).
    #[tokio::test]
    async fn title_sweep_is_limited_to_the_visible_window() {
        // The sender must be wired or the sweep bails before arming anything.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rooms: Vec<RoomDto> = (0..500)
            .map(|i| room_with_activity(Uuid::nil(), &format!("!r{i}:x"), i as i64))
            .collect();
        let mut app = crate::app::App::new(
            crate::api::AxonClient::new("http://127.0.0.1:1".to_owned(), None),
            None,
            crate::config::TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );
        app.set_members_sender(tx);
        app.rooms.rooms = rooms;
        app.rooms.page_size = 20;
        app.rooms.scroll = 0;

        app.sweep_visible_room_titles();

        // The reads are spawned, so count the rooms the sweep armed a per-room
        // cooldown for — one per request it decided to make.
        let armed = app.members_refresh_after.len();
        assert!(
            armed <= 20 + super::ROOM_TITLE_LOOKAHEAD * 2,
            "swept {armed} rooms for a 20-row window; it should track the viewport"
        );
        assert!(armed > 0, "the visible rooms should still be requested");
    }

    /// A room whose members yield no title is recorded, so the sweep stops
    /// asking. Without this it re-requests every cooldown, forever.
    #[test]
    fn a_room_with_no_derivable_title_is_not_asked_again() {
        let mut app = crate::app::App::new(
            crate::api::AxonClient::new("http://127.0.0.1:1".to_owned(), None),
            None,
            crate::config::TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );
        let room = room_with_activity(Uuid::nil(), "!lonely:x", 0);
        let key = RoomKey::from(&room);
        app.rooms.rooms = vec![room];

        // The read landed and named nobody: the account is its only member.
        app.apply_members_outcome(crate::app::timeline::MembersOutcome {
            room_key: key.clone(),
            members: Some(Vec::new()),
        });
        assert!(app.rooms_without_derived_title.contains(&key));

        // A *failed* read says nothing, so it must not be recorded.
        let other = room_with_activity(Uuid::nil(), "!unknown:x", 0);
        let other_key = RoomKey::from(&other);
        app.rooms.rooms.push(other);
        app.apply_members_outcome(crate::app::timeline::MembersOutcome {
            room_key: other_key.clone(),
            members: None,
        });
        assert!(!app.rooms_without_derived_title.contains(&other_key));
    }

    fn room_with_activity(account_id: Uuid, room_id: &str, last_activity_ts: i64) -> RoomDto {
        RoomDto {
            account_id,
            account_user_id: Some("@alice:example.com".to_owned()),
            room_id: room_id.to_owned(),
            name: None,
            topic: None,
            avatar_url: None,
            canonical_alias: None,
            last_activity_ts,
            last_event_id: None,
        }
    }

    #[test]
    fn sort_rooms_by_pin_floats_pinned_then_orders_by_activity() {
        let acct = Uuid::nil();
        // Activity order alone would be: c (3), b (2), a (1).
        let mut rooms = vec![
            room_with_activity(acct, "!a:srv", 1),
            room_with_activity(acct, "!b:srv", 2),
            room_with_activity(acct, "!c:srv", 3),
        ];
        // Pin a then b: pinned section is [a, b] (most recently pinned first means
        // index 0 is the top), so config order here is a, b.
        let pinned = vec![
            RoomKey {
                account_id: acct,
                room_id: "!a:srv".to_owned(),
            },
            RoomKey {
                account_id: acct,
                room_id: "!b:srv".to_owned(),
            },
        ];
        sort_rooms_by_pin(&mut rooms, &pinned, RoomSort::RecentActivity);
        let order: Vec<&str> = rooms.iter().map(|r| r.room_id.as_str()).collect();
        // Pinned a, b first (by pinned position), then unpinned c by activity.
        assert_eq!(order, vec!["!a:srv", "!b:srv", "!c:srv"]);
    }

    #[test]
    fn sort_rooms_by_pin_unpinned_keep_activity_order() {
        let acct = Uuid::nil();
        let mut rooms = vec![
            room_with_activity(acct, "!a:srv", 1),
            room_with_activity(acct, "!b:srv", 5),
            room_with_activity(acct, "!c:srv", 3),
        ];
        sort_rooms_by_pin(&mut rooms, &[], RoomSort::RecentActivity);
        let order: Vec<&str> = rooms.iter().map(|r| r.room_id.as_str()).collect();
        assert_eq!(order, vec!["!b:srv", "!c:srv", "!a:srv"]);
    }

    /// A room carrying a display name, used to exercise alphabetical sorts.
    fn named_room(account_id: Uuid, room_id: &str, name: &str, ts: i64) -> RoomDto {
        let mut room = room_with_activity(account_id, room_id, ts);
        room.name = Some(name.to_owned());
        room
    }

    #[test]
    fn sort_rooms_by_pin_oldest_activity_reverses_order() {
        let acct = Uuid::nil();
        let mut rooms = vec![
            room_with_activity(acct, "!a:srv", 1),
            room_with_activity(acct, "!b:srv", 5),
            room_with_activity(acct, "!c:srv", 3),
        ];
        sort_rooms_by_pin(&mut rooms, &[], RoomSort::OldestActivity);
        let order: Vec<&str> = rooms.iter().map(|r| r.room_id.as_str()).collect();
        assert_eq!(order, vec!["!a:srv", "!c:srv", "!b:srv"]);
    }

    #[test]
    fn sort_rooms_by_pin_alpha_orders_by_title_and_keeps_pins_on_top() {
        let acct = Uuid::nil();
        // Activity order would be zulu, alpha, mike; alpha order is the reverse-ish.
        let mut rooms = vec![
            named_room(acct, "!z:srv", "Zulu", 3),
            named_room(acct, "!a:srv", "alpha", 2),
            named_room(acct, "!m:srv", "Mike", 1),
        ];
        // Pin Mike: it must stay on top regardless of the alphabetical sort.
        let pinned = vec![RoomKey {
            account_id: acct,
            room_id: "!m:srv".to_owned(),
        }];

        sort_rooms_by_pin(&mut rooms, &pinned, RoomSort::AlphaAsc);
        let asc: Vec<&str> = rooms.iter().map(|r| r.room_id.as_str()).collect();
        // Pinned Mike first; then unpinned by case-insensitive name: alpha, Zulu.
        assert_eq!(asc, vec!["!m:srv", "!a:srv", "!z:srv"]);

        sort_rooms_by_pin(&mut rooms, &pinned, RoomSort::AlphaDesc);
        let desc: Vec<&str> = rooms.iter().map(|r| r.room_id.as_str()).collect();
        // Pin still on top; unpinned reversed: Zulu, alpha.
        assert_eq!(desc, vec!["!m:srv", "!z:srv", "!a:srv"]);
    }

    #[test]
    fn sort_rooms_by_pin_alpha_can_use_rendered_titles() {
        let acct = Uuid::nil();
        let mut rooms = vec![
            room_with_activity(acct, "!opaque-b:srv", 2),
            room_with_activity(acct, "!opaque-a:srv", 1),
        ];

        sort_rooms_by_pin_with_title(&mut rooms, &[], RoomSort::AlphaAsc, |room| {
            match room.room_id.as_str() {
                "!opaque-b:srv" => "Alice".to_owned(),
                "!opaque-a:srv" => "Bob".to_owned(),
                _ => room.title().to_owned(),
            }
        });
        let asc: Vec<&str> = rooms.iter().map(|r| r.room_id.as_str()).collect();
        assert_eq!(asc, vec!["!opaque-b:srv", "!opaque-a:srv"]);

        sort_rooms_by_pin_with_title(&mut rooms, &[], RoomSort::AlphaDesc, |room| {
            match room.room_id.as_str() {
                "!opaque-b:srv" => "Alice".to_owned(),
                "!opaque-a:srv" => "Bob".to_owned(),
                _ => room.title().to_owned(),
            }
        });
        let desc: Vec<&str> = rooms.iter().map(|r| r.room_id.as_str()).collect();
        assert_eq!(desc, vec!["!opaque-a:srv", "!opaque-b:srv"]);
    }

    #[test]
    fn older_page_trim_keeps_tail_adjacent_to_existing_events() {
        let mut events: Vec<i32> = (0..50).collect();

        keep_adjacent_older_tail(&mut events, 20);

        assert_eq!(events, (30..50).collect::<Vec<_>>());
    }

    #[test]
    fn is_likely_dm_uses_name_and_alias_heuristic() {
        let acct = Uuid::nil();
        // No name, no alias → treated as a DM.
        assert!(is_likely_dm(&room_with_activity(acct, "!dm:srv", 1)));
        // A blank name is also treated as unnamed.
        let mut blank = room_with_activity(acct, "!blank:srv", 1);
        blank.name = Some("   ".to_owned());
        assert!(is_likely_dm(&blank));
        // A named room is a group.
        assert!(!is_likely_dm(&named_room(acct, "!g:srv", "Team", 1)));
        // An aliased but unnamed room is a group too.
        let mut aliased = room_with_activity(acct, "!al:srv", 1);
        aliased.canonical_alias = Some("#team:srv".to_owned());
        assert!(!is_likely_dm(&aliased));
    }

    fn msg(event_id: &str, body: &str) -> EventDto {
        EventDto {
            account_id: Uuid::nil(),
            event_id: event_id.to_owned(),
            room_id: "!room:localhost".to_owned(),
            sender: "@alice:localhost".to_owned(),
            state_key: None,
            arrival_order: 0,
            origin_ts: 0,
            event_type: "m.room.message".to_owned(),
            content: Some(json!({"msgtype": "m.text", "body": body})),
            body: Some(body.to_owned()),
            relates_to: None,
            redacted: false,
            redaction_event_id: None,
            reactions: None,
            sender_trust: None,
        }
    }

    fn edit(event_id: &str, target_id: &str, new_body: &str) -> EventDto {
        EventDto {
            account_id: Uuid::nil(),
            event_id: event_id.to_owned(),
            room_id: "!room:localhost".to_owned(),
            sender: "@alice:localhost".to_owned(),
            state_key: None,
            arrival_order: 1,
            origin_ts: 1,
            event_type: "m.room.message".to_owned(),
            content: Some(json!({
                "msgtype": "m.text",
                "body": format!("* {new_body}"),
                "m.new_content": {"msgtype": "m.text", "body": new_body},
                "m.relates_to": {"rel_type": "m.replace", "event_id": target_id},
            })),
            body: Some(format!("* {new_body}")),
            relates_to: Some(json!({"rel_type": "m.replace", "event_id": target_id})),
            redacted: false,
            redaction_event_id: None,
            reactions: None,
            sender_trust: None,
        }
    }

    #[test]
    fn apply_edits_updates_original_and_removes_edit_event() {
        let mut events = vec![msg("$orig", "hello"), edit("$edit", "$orig", "hello world")];
        apply_edits(&mut events);
        assert_eq!(events.len(), 1, "edit event should be removed");
        assert_eq!(
            events[0].body.as_deref(),
            Some("hello world"),
            "original body should be replaced with new content"
        );
    }

    #[test]
    fn apply_edits_replaces_formatted_content() {
        let mut original = msg("$orig", "hello");
        original.content = Some(json!({
            "msgtype": "m.text",
            "body": "hello",
            "format": "org.matrix.custom.html",
            "formatted_body": "<em>hello</em>",
        }));
        let mut formatted_edit = edit("$edit", "$orig", "hello world");
        formatted_edit.content = Some(json!({
            "msgtype": "m.text",
            "body": "* hello world",
            "m.new_content": {
                "msgtype": "m.text",
                "body": "hello world",
                "format": "org.matrix.custom.html",
                "formatted_body": "<strong>hello world</strong>",
            },
            "m.relates_to": {"rel_type": "m.replace", "event_id": "$orig"},
        }));
        let mut events = vec![original, formatted_edit];

        apply_edits(&mut events);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].body.as_deref(), Some("hello world"));
        assert_eq!(
            events[0].formatted_body(),
            Some("<strong>hello world</strong>")
        );
    }

    #[test]
    fn apply_edits_leaves_unrelated_messages_untouched() {
        let mut events = vec![
            msg("$a", "first"),
            msg("$b", "second"),
            edit("$edit", "$a", "first edited"),
        ];
        apply_edits(&mut events);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].body.as_deref(), Some("first edited"));
        assert_eq!(events[1].body.as_deref(), Some("second"));
    }

    #[test]
    fn apply_edits_keeps_edit_event_when_original_not_in_page() {
        // If the original message is on an older page that hasn't been loaded,
        // the edit event must remain visible rather than disappearing silently.
        let mut events = vec![msg("$b", "second"), edit("$edit", "$missing", "edited")];
        apply_edits(&mut events);
        // edit event kept (target not in this page); $b untouched
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_id, "$b");
        assert_eq!(events[1].event_id, "$edit");
    }

    #[test]
    fn edit_relation_returns_none_without_relates_to() {
        let event = msg("$m", "body");
        assert!(event.edit_relation().is_none());
    }

    #[test]
    fn edit_relation_returns_none_when_content_missing_new_content() {
        let event = EventDto {
            account_id: Uuid::nil(),
            event_id: "$e".to_owned(),
            room_id: "!r:localhost".to_owned(),
            sender: "@a:localhost".to_owned(),
            state_key: None,
            arrival_order: 0,
            origin_ts: 0,
            event_type: "m.room.message".to_owned(),
            content: Some(json!({"msgtype": "m.text", "body": "* edited"})),
            body: Some("* edited".to_owned()),
            relates_to: Some(json!({"rel_type": "m.replace", "event_id": "$orig"})),
            redacted: false,
            redaction_event_id: None,
            reactions: None,
            sender_trust: None,
        };
        // relates_to is set but content has no m.new_content → None
        assert!(event.edit_relation().is_none());
    }
}
