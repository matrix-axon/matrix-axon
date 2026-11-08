//! Pending-invite inbox (ADR 0130).
//!
//! Selectable room rows stay joined rooms. A filtered count above zero appends
//! ` · N invites` to the rooms title. `/pending` opens the popup.
//!
//! The account on a live frame is the WebSocket envelope's. `invite.removed`
//! carries only `room_id`. `GET /v1/invites` never sends `account_id`.
//! A 404 is an older server: an empty inbox and no error.
//!
//! An accept or reject that returns 2xx is suppressed until a later list omits
//! that key, so an in-flight list cannot put the row back. A 2xx also asks for
//! a fresh list, so a later re-invite is not hidden by that suppression when
//! its `invite.added` frame was missed. A reject asks for confirmation first.
//! A reject that returns 504 uses the leave schedule and settles from this list.
//! An accept that returns 504 re-reads this list on the short join-watch delays.

use std::collections::HashSet;
use std::fmt::Write;
use std::ops::Range;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use uuid::Uuid;

use crate::api::{ApiError, InviteDto, InviteListSnapshot};
use crate::wrap::{rich_lines_to_spans, wrap_rich_lines, RichSpan};

use super::room_actions::{
    MembershipStep, PendingMembership, RoomActionFailure, JOIN_OPEN_READ_DELAYS,
    PENDING_MEMBERSHIP_CHECK_DELAYS,
};
use super::timeline::contains_ascii_case_insensitive;
use super::{
    account_localpart, App, BootstrapOutcome, Mode, PopupKind, RoomFilter, RoomKey, Status,
};

/// Extra invite-list reads after an accept returned 504.
///
/// The outcome handler already requested one read. `reads_remaining` counts
/// the delayed ones, on [`JOIN_OPEN_READ_DELAYS`].
#[derive(Debug, Clone)]
pub(crate) struct InviteAcceptFollowUp {
    pub(crate) reads_remaining: u8,
    pub(crate) next_read_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InviteVerb {
    Accept,
    Reject,
}

#[derive(Debug)]
pub(crate) struct InviteActionOutcome {
    pub(crate) invite: InviteDto,
    pub(crate) verb: InviteVerb,
    pub(crate) result: Result<Option<String>, RoomActionFailure>,
}

impl App {
    pub(crate) fn set_invite_action_sender(
        &mut self,
        tx: tokio::sync::mpsc::UnboundedSender<InviteActionOutcome>,
    ) {
        self.invite_action_tx = Some(tx);
    }

    /// Ask for a fresh invite list, coalescing concurrent requests.
    ///
    /// A request behind one already in flight only remembers itself. That
    /// in-flight response still merges live adds and removals.
    /// Dispatch does not clear those maps. A failed read leaves them
    /// populated, and the retry must merge them or its snapshot can put a
    /// removed invite back or drop an add from the failed read. A successful
    /// read already took the maps, so the next dispatch starts empty.
    pub(crate) fn request_invites_refresh(&mut self) {
        if self.invites_fetch_inflight {
            self.invites_fetch_again = true;
            return;
        }
        self.invites_fetch_inflight = true;
        let client = self.client.clone();
        self.spawn_bootstrap(async move { BootstrapOutcome::Invites(client.list_invites().await) });
    }

    pub(super) fn open_invites(&mut self) {
        self.sync_invite_selection();
        self.open_popup(PopupKind::Invites);
        self.request_invites_refresh();
    }

    pub(crate) fn append_invite_count(&self, title: &mut String) {
        let count = self.filtered_invites().count();
        if count == 0 {
            return;
        }
        let noun = if count == 1 { "invite" } else { "invites" };
        let _ = write!(title, " · {count} {noun}");
    }

    /// Title of the inbox popup, using the configured accept, reject, and close keys.
    ///
    /// Reject is omitted when that key cannot reach the popup. Submit is
    /// checked first, and shortcuts handled before the popup's own keys win
    /// as well. A page binding is checked after reject, so it still appears.
    pub(crate) fn invites_popup_title(&self) -> String {
        let accept = self.shortcuts.submit.label();
        let close = self.shortcuts.clear_input.label();
        if self.reject_invite_reaches_popup() {
            format!(
                "Invites  ({} accept, {} reject, {} close)",
                accept,
                self.shortcuts.reject_invite.label(),
                close,
            )
        } else {
            format!("Invites  ({accept} accept, {close} close)")
        }
    }

    /// Whether `reject_invite` is the key the inbox popup will act on.
    fn reject_invite_reaches_popup(&self) -> bool {
        let reject = &self.shortcuts.reject_invite;
        // `handle_key` runs the first group before the mode match, and the
        // popup is not a prompt, so none of them are hidden while it is open.
        // `handle_popup_key` then closes on `clear_input` before the inbox
        // sees the key. Submit is first inside the inbox itself.
        let earlier = [
            &self.shortcuts.quit,
            &self.shortcuts.focus_next,
            &self.shortcuts.focus_prev,
            &self.shortcuts.next_room,
            &self.shortcuts.previous_room,
            &self.shortcuts.next_account,
            &self.shortcuts.previous_account,
            &self.shortcuts.message_down,
            &self.shortcuts.message_up,
            &self.shortcuts.toggle_accounts_panel,
            &self.shortcuts.toggle_rooms_panel,
            &self.shortcuts.toggle_unread_filter,
            &self.shortcuts.room_filter_cycle,
            &self.shortcuts.room_sort_cycle,
            &self.shortcuts.room_filter_dms,
            &self.shortcuts.room_filter_groups,
            &self.shortcuts.room_filter_favorites,
            &self.shortcuts.room_filter_all,
            &self.shortcuts.room_filter_by_name,
            &self.shortcuts.room_sort_recent,
            &self.shortcuts.room_sort_alpha,
            &self.shortcuts.unread_threads,
            &self.shortcuts.refresh,
            &self.shortcuts.clear_input,
            &self.shortcuts.submit,
        ];
        earlier.into_iter().all(|binding| binding != reject)
    }

    /// `Reject {title}? [y/N]` for the invite the question was asked about.
    ///
    /// A row that has already left falls back to its room id, so the question
    /// still names something.
    pub(crate) fn invite_reject_question(&self) -> String {
        let Mode::ConfirmInviteReject { key } = &self.mode else {
            return "Reject invite? [y/N]".to_owned();
        };
        let subject = self
            .invites
            .iter()
            .find(|invite| invite_is_key(invite, key))
            .map(|invite| invite.title().to_owned())
            .unwrap_or_else(|| key.room_id.clone());
        format!("Reject {subject}? [y/N]")
    }

    /// Invites the rooms title and the popup both show.
    ///
    /// Account filtering matches the room list: inactive accounts, the launch
    /// account, and the accounts-panel selection. The room filter then applies.
    /// Favorites show none. Unread keeps every invite that passed the account
    /// filter, because a pending invite is unseen attention.
    pub(crate) fn filtered_invites(&self) -> impl Iterator<Item = &InviteDto> {
        let inactive = &self.accounts.inactive_ids;
        let launch = self.account_filter;
        let active = self.active_account_filter();
        let filter = &self.room_filter;
        self.invites.iter().filter(move |invite| {
            invite_passes_account(invite, inactive, launch, active)
                && invite_passes_room_filter(invite, filter)
        })
    }

    pub(super) fn apply_invite_fetch(&mut self, result: Result<InviteListSnapshot, ApiError>) {
        match result {
            Ok(snapshot) => self.apply_invite_snapshot(snapshot),
            Err(err) => {
                // A failed read keeps the last list and says nothing about a
                // pending reject. Live maps stay so the retry can merge them.
                if !self.is_mid_command() {
                    self.status = Status::from(format!("invite refresh failed: {err}"));
                }
            }
        }
    }

    fn apply_invite_snapshot(&mut self, snapshot: InviteListSnapshot) {
        let live_adds = std::mem::take(&mut self.invite_live_adds);
        let live_removals = std::mem::take(&mut self.invite_live_removals);
        if snapshot.not_found {
            // Older server. Clearing the visible list is the inbox. Suppression
            // and a pending reject stay: this body is not positive absence.
            self.invites.clear();
            self.sync_invite_selection();
            return;
        }
        let server_keys: HashSet<RoomKey> = snapshot.invites.iter().map(RoomKey::from).collect();
        self.invite_suppressed
            .retain(|key| server_keys.contains(key));
        let mut merged = Vec::new();
        let mut merged_keys = HashSet::new();
        for (key, invite) in live_adds {
            if server_keys.contains(&key)
                || self.invite_suppressed.contains(&key)
                || live_removals.contains(&key)
                || !merged_keys.insert(key)
            {
                continue;
            }
            merged.push(invite);
        }
        for invite in snapshot.invites {
            let key = RoomKey::from(&invite);
            if self.invite_suppressed.contains(&key) || live_removals.contains(&key) {
                continue;
            }
            if merged_keys.insert(key) {
                merged.push(invite);
            }
        }
        let present: HashSet<RoomKey> = merged.iter().map(RoomKey::from).collect();
        self.invites = merged;
        self.settle_pending_invite_rejects(&present, snapshot.started);
        self.sync_invite_selection();
    }

    pub(super) fn note_invite_added(&mut self, invite: InviteDto) {
        let key = RoomKey::from(&invite);
        // A re-invite must show. Adding does not by itself end a pending reject.
        self.invite_suppressed.remove(&key);
        self.invite_live_removals.remove(&key);
        self.invites.retain(|row| !invite_is_key(row, &key));
        self.invites.insert(0, invite.clone());
        if self.invites_fetch_inflight {
            self.invite_live_adds
                .retain(|(existing, _)| existing != &key);
            self.invite_live_adds.insert(0, (key, invite));
        }
        self.sync_invite_selection();
    }

    pub(super) fn note_invite_removed(&mut self, account_id: Uuid, room_id: &str) {
        let key = RoomKey {
            account_id,
            room_id: room_id.to_owned(),
        };
        self.invites.retain(|row| !invite_is_key(row, &key));
        self.invite_live_adds
            .retain(|(existing, _)| existing != &key);
        if self.invites_fetch_inflight {
            self.invite_live_removals.insert(key.clone());
        }
        // A removal does not suppress. The next list is free to show a re-invite
        // that arrives without its own `invite.added` frame.
        self.end_pending_invite_reject(&key, true);
        self.sync_invite_selection();
    }

    /// 2xx accept or reject. The outcome handler sets the status line.
    fn drop_invite_locally(&mut self, invite: &InviteDto) {
        let key = RoomKey::from(invite);
        self.invite_suppressed.insert(key.clone());
        self.invites.retain(|row| !invite_is_key(row, &key));
        self.invite_live_adds
            .retain(|(existing, _)| existing != &key);
        if self.invites_fetch_inflight {
            self.invite_live_removals.insert(key.clone());
        }
        self.pending_invite_rejects.remove(&key);
        self.sync_invite_selection();
    }

    fn end_pending_invite_reject(&mut self, key: &RoomKey, announce: bool) {
        let Some(pending) = self.pending_invite_rejects.remove(key) else {
            return;
        };
        if announce && !pending.room_title.is_empty() && !self.is_mid_command() {
            let title = pending.room_title;
            self.status = Status::from(format!("rejected {title}"));
        }
    }

    fn settle_pending_invite_rejects(&mut self, present: &HashSet<RoomKey>, started: Instant) {
        let mut settled = Vec::new();
        for (key, pending) in &mut self.pending_invite_rejects {
            // A read that started before the 504 cannot speak for it.
            if started < pending.since {
                continue;
            }
            // The GET is unfiltered, so a successful list covers every account
            // that is still active. A logged-out account's rows are absent
            // because that account is gone, not because the reject landed.
            // Skip it entirely: do not mark it seen and do not end the hold.
            // A display filter is not this check. Settlement uses `present`,
            // which is the merged list before the room filter.
            if self.accounts.inactive_ids.contains(&key.account_id) {
                continue;
            }
            if present.contains(key) {
                pending.seen_listed = true;
            } else {
                settled.push((key.clone(), pending.room_title.clone()));
            }
        }
        for (key, title) in settled {
            self.pending_invite_rejects.remove(&key);
            if !self.is_mid_command() {
                self.status = Status::from(format!("rejected {title}"));
            }
        }
    }

    fn begin_pending_invite_reject(&mut self, invite: &InviteDto, now: Instant) {
        self.pending_invite_rejects.insert(
            RoomKey::from(invite),
            PendingMembership {
                room_title: invite.title().to_owned(),
                since: now,
                step: 0,
                next_check: now + PENDING_MEMBERSHIP_CHECK_DELAYS[0],
                retryable: false,
                seen_listed: false,
            },
        );
    }

    /// Same one-step-per-tick schedule as a pending leave.
    ///
    /// The schedule itself is [`PendingMembership::advance`]. Each due tick
    /// asks for one invite-list read. Settlement is
    /// [`Self::settle_pending_invite_rejects`], from that list.
    pub(crate) fn poll_pending_invite_rejects(&mut self, now: Instant) {
        let mut refresh = false;
        let mut expired = Vec::new();
        // A list read cannot settle a logged-out account, so leave its
        // schedule where it is and do not ask for one. The next tick after
        // the account is active again takes one overdue step.
        let inactive = self.accounts.inactive_ids.clone();
        for (key, pending) in &mut self.pending_invite_rejects {
            if inactive.contains(&key.account_id) {
                continue;
            }
            match pending.advance(now) {
                Some(MembershipStep::Advance) => refresh = true,
                Some(MembershipStep::Expired) => expired.push(key.clone()),
                None => {}
            }
        }
        for key in expired {
            let Some(pending) = self.pending_invite_rejects.remove(&key) else {
                continue;
            };
            if pending.seen_listed && !self.is_mid_command() {
                self.status = Status::from(format!(
                    "{} is still invited; the reject may not have gone through, so reject it again",
                    pending.room_title
                ));
            }
        }
        if refresh {
            self.request_invites_refresh();
        }
    }

    /// Ask for the delayed invite-list reads that follow an accept 504.
    ///
    /// The immediate read is requested by the outcome handler. This arms the
    /// two that follow, on the same delays as a join that was not listed yet.
    fn arm_invite_accept_followup(&mut self) {
        let now = Instant::now();
        self.invite_accept_followup = Some(InviteAcceptFollowUp {
            reads_remaining: JOIN_OPEN_READ_DELAYS.len() as u8,
            next_read_at: now + JOIN_OPEN_READ_DELAYS[0],
        });
    }

    /// Request one due invite-list read after an accept 504. Called from the
    /// main-loop tick. One step per tick, the same way a join watch does.
    pub(crate) fn poll_invite_accept_followup(&mut self, now: Instant) {
        let Some(followup) = self.invite_accept_followup.as_mut() else {
            return;
        };
        if followup.reads_remaining == 0 || now < followup.next_read_at {
            return;
        }
        followup.reads_remaining -= 1;
        let done = followup.reads_remaining == 0;
        if !done {
            let delay_index = JOIN_OPEN_READ_DELAYS.len() - usize::from(followup.reads_remaining);
            followup.next_read_at = now + JOIN_OPEN_READ_DELAYS[delay_index];
        }
        if done {
            self.invite_accept_followup = None;
        }
        self.request_invites_refresh();
    }

    pub(crate) fn handle_invites_popup_key(&mut self, key: KeyEvent) {
        // Submit is checked first. A reject binding of the same key accepts.
        if self.shortcuts.submit.matches(key) {
            self.accept_selected_invite();
            return;
        }
        if self.shortcuts.reject_invite.matches(key) {
            self.prompt_invite_reject();
            return;
        }
        if key.code == KeyCode::Up {
            self.move_invite_selection(-1);
        } else if key.code == KeyCode::Down {
            self.move_invite_selection(1);
        } else if key.code == KeyCode::PageUp || self.shortcuts.message_page_up.matches(key) {
            self.move_invite_selection(-8);
        } else if key.code == KeyCode::PageDown || self.shortcuts.message_page_down.matches(key) {
            self.move_invite_selection(8);
        }
    }

    /// `y` rejects the invite that was selected when the question was asked.
    /// `n` and Esc return to the inbox. The composer is left as it was.
    pub(crate) fn handle_confirm_invite_reject_key(&mut self, key: KeyEvent, room_key: RoomKey) {
        if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            let still_pending = self
                .invites
                .iter()
                .any(|invite| invite_is_key(invite, &room_key));
            self.mode = Mode::Popup(PopupKind::Invites);
            if !still_pending {
                // The row left while the question was open. Do not send, and
                // do not move the marker onto a different invite.
                self.status = Status::from("that invite is no longer pending".to_owned());
                return;
            }
            // Still in the full list, including when a filter would hide it.
            // The user confirmed this key.
            self.invite_selection = Some(room_key);
            self.start_invite_action(InviteVerb::Reject);
        } else if matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N'))
            || self.shortcuts.clear_input.matches(key)
        {
            self.mode = Mode::Popup(PopupKind::Invites);
            self.status = Status::from("reject canceled".to_owned());
        }
    }

    fn accept_selected_invite(&mut self) {
        // A filter can hide the stored row without a redraw. Move the marker
        // and wait for another key. Do not accept whatever is now first.
        if !self.retarget_hidden_invite_selection() {
            return;
        }
        self.start_invite_action(InviteVerb::Accept);
    }

    fn prompt_invite_reject(&mut self) {
        if !self.retarget_hidden_invite_selection() {
            return;
        }
        let Some(key) = self.invite_selection.clone() else {
            self.status = Status::from("no invite selected".to_owned());
            return;
        };
        // A hold that has not reached its first check is not a second leave.
        // Ask nothing: the pending status is the answer, and the join watch
        // stays where it was.
        if let Some(pending) = self.pending_invite_rejects.get(&key) {
            if !pending.retryable {
                let title = pending.room_title.clone();
                self.status = Status::from(pending_reject_status(&title));
                return;
            }
        }
        if self.selected_invite().is_none() {
            self.status = Status::from("no invite selected".to_owned());
            return;
        }
        self.mode = Mode::ConfirmInviteReject { key };
        self.status = Status::from(self.invite_reject_question());
    }

    /// True when the stored row is still in the filtered list.
    ///
    /// Otherwise the selection moves to a row that is still visible, and this
    /// key does not accept or reject. An empty list says nothing is selected.
    fn retarget_hidden_invite_selection(&mut self) -> bool {
        if self.invite_selection_visible() {
            return true;
        }
        self.sync_invite_selection();
        if self.invite_selection.is_none() {
            self.status = Status::from("no invite selected".to_owned());
        }
        false
    }

    fn invite_selection_visible(&self) -> bool {
        let Some(key) = self.invite_selection.clone() else {
            return false;
        };
        self.filtered_invites()
            .any(|invite| invite_is_key(invite, &key))
    }

    fn move_invite_selection(&mut self, delta: isize) {
        self.sync_invite_selection();
        let filtered: Vec<&InviteDto> = self.filtered_invites().collect();
        if filtered.is_empty() {
            self.invite_selection = None;
            return;
        }
        let current = self
            .invite_selection
            .as_ref()
            .and_then(|key| {
                filtered
                    .iter()
                    .position(|invite| invite_is_key(invite, key))
            })
            .unwrap_or(0);
        let next = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current
                .saturating_add(delta as usize)
                .min(filtered.len() - 1)
        };
        self.invite_selection = Some(RoomKey::from(filtered[next]));
    }

    pub(crate) fn sync_invite_selection(&mut self) {
        let current = self.invite_selection.clone();
        let still = current.as_ref().is_some_and(|key| {
            self.filtered_invites()
                .any(|invite| invite_is_key(invite, key))
        });
        if still {
            return;
        }
        let next = self.filtered_invites().next().map(RoomKey::from);
        self.invite_selection = next;
    }

    fn selected_invite(&self) -> Option<&InviteDto> {
        let key = self.invite_selection.as_ref()?;
        self.invites
            .iter()
            .find(|invite| invite_is_key(invite, key))
    }

    fn close_invite_popup(&mut self) {
        if self.mode == Mode::Popup(PopupKind::Invites) {
            self.mode = Mode::Compose;
            self.popup_scroll = 0;
        }
    }

    pub(crate) fn start_invite_action(&mut self, verb: InviteVerb) {
        let Some(invite) = self.selected_invite().cloned() else {
            self.status = Status::from("no invite selected".to_owned());
            return;
        };
        let key = RoomKey::from(&invite);
        if verb == InviteVerb::Reject {
            if let Some(pending) = self.pending_invite_rejects.get(&key) {
                if !pending.retryable {
                    self.status = Status::from(pending_reject_status(&pending.room_title));
                    return;
                }
            }
        }
        if self.room_action_busy {
            self.status = Status::from("a room action is already in progress".to_owned());
            return;
        }
        let Some(tx) = self.invite_action_tx.clone() else {
            return;
        };
        // The action will run. A late join watch must not open another room
        // under it. A held reject and a busy action leave that watch.
        self.pending_join_open = None;
        self.deferred_join_failure = None;
        let title = invite.title().to_owned();
        self.status = Status::from(match verb {
            InviteVerb::Accept => format!("accepting {title}"),
            InviteVerb::Reject => format!("rejecting {title}"),
        });
        self.room_action_busy = true;
        let client = self.client.clone();
        tokio::spawn(async move {
            let result = match verb {
                InviteVerb::Accept => client
                    .join_room(invite.account_id, &invite.room_id, &[])
                    .await
                    .map(Some),
                InviteVerb::Reject => client
                    .leave_room(invite.account_id, &invite.room_id)
                    .await
                    .map(|()| None),
            }
            .map_err(|err| RoomActionFailure {
                timed_out: err.is_gateway_timeout(),
                message: err.to_string(),
            });
            let _ = tx.send(InviteActionOutcome {
                invite,
                verb,
                result,
            });
        });
    }

    pub(crate) async fn handle_invite_action_outcome(&mut self, outcome: InviteActionOutcome) {
        self.room_action_busy = false;
        let title = outcome.invite.title().to_owned();
        let account_id = outcome.invite.account_id;
        let room_id = outcome.invite.room_id.clone();
        match (outcome.verb, outcome.result) {
            (InviteVerb::Accept, Ok(resolved)) => {
                self.drop_invite_locally(&outcome.invite);
                // Suppression stays until a later list omits the key. Ask for
                // that list now, the same way a 2xx reject does, so a re-invite
                // that arrives without `invite.added` is not hidden until restart.
                self.request_invites_refresh();
                self.close_invite_popup();
                let target = resolved.unwrap_or(room_id);
                self.finish_invite_accept(account_id, &target, &title, false)
                    .await;
            }
            (InviteVerb::Accept, Err(failure)) if failure.timed_out => {
                // A 504 follows join for the room list, and also re-reads the
                // invite list now and twice more. The row stays until one of
                // those reads, or `invite.removed`, drops it. Do not suppress.
                self.close_invite_popup();
                self.request_invites_refresh();
                self.arm_invite_accept_followup();
                self.finish_invite_accept(account_id, &room_id, &title, true)
                    .await;
            }
            (InviteVerb::Accept, Err(err)) => {
                self.status = Status::from(format!("accept failed for {title}: {err}"));
            }
            (InviteVerb::Reject, Ok(_)) => {
                self.drop_invite_locally(&outcome.invite);
                self.request_invites_refresh();
                if !self.is_mid_command() {
                    self.status = Status::from(format!("rejected {title}"));
                }
            }
            (InviteVerb::Reject, Err(failure)) if failure.timed_out => {
                self.begin_pending_invite_reject(&outcome.invite, Instant::now());
                self.status = Status::from(pending_reject_status(&title));
            }
            (InviteVerb::Reject, Err(err)) => {
                let key = RoomKey::from(&outcome.invite);
                self.pending_invite_rejects.remove(&key);
                self.status = Status::from(format!("reject failed for {title}: {err}"));
            }
        }
    }

    /// Popup body. Each invite's wrapped rows are one range, so scroll can
    /// bring the whole selected row into view.
    ///
    /// The third value is the selected row's index in the visible list.
    /// Both the empty-state count and the rows come from one account-filter pass.
    pub(crate) fn invite_popup_lines(
        &self,
        width: usize,
    ) -> (Vec<Line<'static>>, Vec<Range<usize>>, Option<usize>) {
        let inactive = &self.accounts.inactive_ids;
        let launch = self.account_filter;
        let active = self.active_account_filter();
        let show_account = active.is_none() && self.accounts_panel_visible();
        let selected_color = self.colors.selected_room;
        let hint_color = self.colors.input_hint;
        let width = width.max(1);
        let mut account_count = 0usize;
        let mut lines = Vec::new();
        let mut ranges = Vec::new();
        let mut selected_index = None;
        let mut visible_index = 0usize;
        for invite in self
            .invites
            .iter()
            .filter(|invite| invite_passes_account(invite, inactive, launch, active))
        {
            account_count += 1;
            if !invite_passes_room_filter(invite, &self.room_filter) {
                continue;
            }
            let start = lines.len();
            let selected = self
                .invite_selection
                .as_ref()
                .is_some_and(|key| invite_is_key(invite, key));
            if selected {
                selected_index = Some(visible_index);
            }
            let pending = self
                .pending_invite_rejects
                .keys()
                .any(|key| invite_is_key(invite, key));
            let row = invite_row_spans(
                invite,
                selected,
                show_account,
                pending,
                selected_color,
                hint_color,
            );
            for spans in rich_lines_to_spans(wrap_rich_lines(vec![row], width, width)) {
                lines.push(Line::from(spans));
            }
            ranges.push(start..lines.len());
            visible_index += 1;
        }
        if visible_index == 0 {
            let message = if account_count == 0 {
                "No invites."
            } else {
                "No invites match this filter."
            };
            return (vec![Line::from(message)], Vec::new(), None);
        }
        (lines, ranges, selected_index)
    }

    pub(crate) fn reveal_selected_invite(
        &mut self,
        ranges: &[Range<usize>],
        selected: Option<usize>,
        page_size: usize,
    ) {
        let Some(range) = selected.and_then(|index| ranges.get(index)) else {
            return;
        };
        if range.start < self.popup_scroll {
            self.popup_scroll = range.start;
        } else if range.end > self.popup_scroll.saturating_add(page_size) {
            self.popup_scroll = range.end.saturating_sub(page_size);
        }
    }
}

fn invite_is_key(invite: &InviteDto, key: &RoomKey) -> bool {
    invite.account_id == key.account_id && invite.room_id == key.room_id
}

fn pending_reject_status(title: &str) -> String {
    format!(
        "your homeserver hasn't finished rejecting {title} yet; it will drop off this list once it has, so there's no need to reject it again"
    )
}

fn invite_passes_account(
    invite: &InviteDto,
    inactive: &HashSet<Uuid>,
    launch: Option<Uuid>,
    active: Option<Uuid>,
) -> bool {
    if inactive.contains(&invite.account_id) {
        return false;
    }
    if launch.is_some_and(|id| id != invite.account_id) {
        return false;
    }
    if active.is_some_and(|id| id != invite.account_id) {
        return false;
    }
    true
}

fn invite_passes_room_filter(invite: &InviteDto, filter: &RoomFilter) -> bool {
    match filter {
        RoomFilter::All | RoomFilter::Unread => true,
        RoomFilter::Dms => invite.is_direct,
        RoomFilter::Groups => !invite.is_direct,
        RoomFilter::Favorites => false,
        RoomFilter::Name(query) => [
            invite.title(),
            invite.canonical_alias.as_deref().unwrap_or(""),
            invite.room_id.as_str(),
            invite.inviter_user_id.as_str(),
            invite.inviter_display_name.as_deref().unwrap_or(""),
        ]
        .into_iter()
        .any(|field| contains_ascii_case_insensitive(field, query)),
    }
}

fn invite_row_spans(
    invite: &InviteDto,
    selected: bool,
    show_account: bool,
    pending: bool,
    selected_color: ratatui::style::Color,
    hint_color: ratatui::style::Color,
) -> Vec<RichSpan> {
    let marker = if selected { "> " } else { "  " };
    let title_style = if selected {
        Style::default()
            .fg(selected_color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let inviter = invite
        .inviter_display_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(invite.inviter_user_id.as_str());
    let mut spans = vec![
        RichSpan::new(marker, Style::default()),
        RichSpan::new(invite.title().to_owned(), title_style),
        RichSpan::new(format!(" from {inviter}"), Style::default()),
    ];
    if show_account {
        let local =
            account_localpart(&invite.account_user_id).unwrap_or(invite.account_user_id.as_str());
        spans.push(RichSpan::new(format!(" [{local}]"), Style::default()));
    }
    if pending {
        spans.push(RichSpan::new(
            " (reject pending)",
            Style::default().fg(hint_color),
        ));
    }
    spans
}
