use std::collections::HashMap;

use super::completion::emoji_cycle_status;
use super::{cycle_index, App, Mode, OwnReaction, RoomKey, Status};

/// Record one sender's reaction against a tally, idempotently.
///
/// The single answer to "is this reaction already counted", shared by the
/// optimistic local path and the live remote one. Each previously had its own
/// guard over its own field — local checked `!me`, remote checked `senders` —
/// and the two disagreed whenever the account's user id was unresolved, or the
/// paths ran in either order. That mismatch is the root cause behind all three
/// counting bugs reported on #220. `count` is now guarded by exactly one thing,
/// sender membership, and `me` / `my_event_ids` are set in the same step rather
/// than by whichever path happened to run.
///
/// `reaction_event_id` is recorded only when the reaction is ours, because it is
/// what a withdrawal redacts: `own_reactions_for` needs `me` *and* an id.
fn record_reaction(
    tally: &mut crate::api::ReactionTally,
    sender: Option<&str>,
    reaction_event_id: Option<&str>,
    is_own: bool,
) {
    match sender {
        Some(sender) if !tally.senders.iter().any(|known| known == sender) => {
            tally.senders.push(sender.to_owned());
            // `me` is set only by this function, and only alongside counting
            // us exactly once. So `is_own && me` here means the fallback arm
            // already counted us, back when we could not be named: adopt the
            // sender entry now that we can, but do not count the same person
            // again. Without this, learning our own id part-way through a
            // session doubled a badge we had already contributed to (#220
            // review, pass 4).
            if !(is_own && tally.me) {
                tally.count += 1;
            }
        }
        Some(_) => {}
        // A sender we cannot name cannot be deduplicated by name. That happens
        // only for our own reaction on a client that has not resolved its user
        // id; `my_event_ids` and `me` stand in, and the caller's event-id check
        // catches the echo, so this still counts exactly once.
        None if is_own && !tally.me && tally.my_event_ids.is_empty() => {
            tally.count += 1;
        }
        None => {}
    }
    if is_own {
        tally.me = true;
        if let Some(id) = reaction_event_id {
            if !tally.my_event_ids.iter().any(|known| known == id) {
                tally.my_event_ids.push(id.to_owned());
            }
        }
    }
}

impl App {
    pub(crate) async fn start_unreact_from_selected_message(&mut self) {
        let Some(target_event_id) = self.selected_message_id().map(str::to_owned) else {
            self.status = Status::from("select a displayed message before unreacting".to_owned());
            return;
        };
        let choices = match self.own_reactions_for(&target_event_id) {
            Ok(choices) => choices,
            Err(message) => {
                self.status = Status::from(message);
                return;
            }
        };
        match choices.as_slice() {
            [] => {
                self.status = Status::from("you have no reactions on this message".to_owned());
            }
            [only] => {
                self.withdraw_reaction(&target_event_id, only.clone()).await;
            }
            _ => {
                self.status = unreact_selection_status(&choices, 0);
                self.mode = Mode::Unreacting {
                    target_event_id,
                    choices,
                    selected: 0,
                };
            }
        }
    }

    pub(super) fn own_reactions_for(
        &self,
        target_event_id: &str,
    ) -> Result<Vec<OwnReaction>, String> {
        // The withdrawable reaction ids come from the server-aggregated tally on
        // the target message (`my_event_ids`), not from scanning raw reaction
        // events — the collapsed timeline (M8) no longer carries them.
        let Some(target) = self
            .selected_raw_events()
            .iter()
            .find(|event| event.event_id == target_event_id)
        else {
            return Ok(Vec::new());
        };
        let Some(reactions) = &target.reactions else {
            return Ok(Vec::new());
        };
        let mut choices: Vec<_> = reactions
            .iter()
            .filter(|(_, tally)| tally.me && !tally.my_event_ids.is_empty())
            .map(|(key, tally)| OwnReaction {
                key: key.clone(),
                event_ids: tally.my_event_ids.clone(),
            })
            .collect();
        choices.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(choices)
    }

    pub(crate) async fn withdraw_reaction(&mut self, target_event_id: &str, reaction: OwnReaction) {
        let Some(room) = self.selected_room().cloned() else {
            self.status = Status::from("no room selected".to_owned());
            return;
        };
        let total = reaction.event_ids.len();
        // Track which redactions actually landed so the optimistic tally update
        // reflects partial progress on failure.
        let mut withdrawn: Vec<String> = Vec::with_capacity(total);
        for (index, event_id) in reaction.event_ids.iter().enumerate() {
            match self
                .client
                .redact_event(room.account_id, &room.room_id, event_id, None)
                .await
            {
                Ok(_) => withdrawn.push(event_id.clone()),
                Err(err) => {
                    self.remove_local_reaction(target_event_id, &reaction.key, &withdrawn);
                    self.status = Status::Info(format!(
                        "unreact failed after {index}/{total} withdrawals: {err}"
                    ));
                    return;
                }
            }
        }
        self.remove_local_reaction(target_event_id, &reaction.key, &withdrawn);
        self.status = Status::EventAction {
            debug: format!("withdrew reaction {}", reaction.key),
            redacted: "withdrew reaction",
        };
    }

    /// Optimistically drop the account user's own reaction(s) for `key` from the
    /// target message's server-aggregated tally. The collapsed timeline (M8) no
    /// longer carries raw `m.reaction` rows, so redacting them does not update the
    /// on-row aggregate the TUI now renders from — without this patch the badge
    /// and unreact choice would persist (and re-redact already-redacted ids) until
    /// the next full reload. Removing the owner's last id clears `me` and drops the
    /// owner's single distinct-sender contribution from `count`; a key with no
    /// remaining count is removed entirely.
    pub(super) fn remove_local_reaction(
        &mut self,
        target_event_id: &str,
        key: &str,
        withdrawn: &[String],
    ) {
        if withdrawn.is_empty() {
            return;
        }
        let Some(room) = self.selected_room().cloned() else {
            return;
        };
        let own_user_id = self.own_user_id_for(&room);
        let room_key = RoomKey::from(&room);
        let Some(events) = self.messages.events.get_mut(&room_key) else {
            return;
        };
        let Some(event) = events.iter_mut().find(|e| e.event_id == target_event_id) else {
            return;
        };
        let Some(reactions) = event.reactions.as_mut() else {
            return;
        };
        if let Some(tally) = reactions.get_mut(key) {
            tally.my_event_ids.retain(|id| !withdrawn.contains(id));
            if tally.my_event_ids.is_empty() && tally.me {
                tally.me = false;
                // `record_reaction` counts our contribution exactly once --
                // through the sender arm when we can name ourselves, through
                // the fallback arm when we cannot -- so removing it decrements
                // exactly once, whichever arm recorded it. Gating the decrement
                // on finding ourselves in `senders` left a phantom count
                // whenever the fallback arm had recorded us and our user id
                // resolved before the withdrawal (#220 review, pass 3).
                //
                // Dropping the sender entry stays best-effort for the same
                // reason: there is nothing to drop when the fallback arm ran.
                tally.count = tally.count.saturating_sub(1);
                if let Some(own) = own_user_id.as_deref() {
                    tally.senders.retain(|sender| sender != own);
                }
            }
            if tally.count <= 0 {
                reactions.remove(key);
            }
        }
        if reactions.is_empty() {
            event.reactions = None;
        }
    }

    /// Our own Matrix user id, inferred from reaction events already known to
    /// be ours.
    ///
    /// The fourth tier of identity resolution, after `own_user_id_for`'s three,
    /// and the only one left when the account's user id is unknown by every
    /// other route: a reaction we recorded as ours names us in its own raw row.
    ///
    /// Shared by both reaction paths deliberately. It began as a local-only
    /// fallback, and the remote path not having it meant the two disagreed
    /// about what "ours" meant — a second device's reaction with the same key
    /// then counted as a new sender, doubling a badge for one person (#220
    /// review, pass 4). Callers differ only in which ids they can offer: the
    /// local path the one it is about to record, the remote path the ones the
    /// tally already holds.
    fn own_user_id_from_reaction_rows(
        &self,
        room: &RoomKey,
        reaction_event_ids: &[&str],
    ) -> Option<String> {
        if reaction_event_ids.is_empty() {
            return None;
        }
        self.messages.events.get(room).and_then(|events| {
            events
                .iter()
                .find(|event| reaction_event_ids.contains(&event.event_id.as_str()))
                .map(|event| event.sender.clone())
        })
    }

    /// The reaction event ids this tally already records as ours.
    fn own_reaction_ids(&self, room: &RoomKey, target_event_id: &str, key: &str) -> Vec<String> {
        self.messages
            .events
            .get(room)
            .and_then(|events| {
                let target = events
                    .iter()
                    .find(|event| event.event_id == target_event_id)?;
                Some(target.reactions.as_ref()?.get(key)?.my_event_ids.clone())
            })
            .unwrap_or_default()
    }

    /// Optimistically add the account user's own reaction to the target message's
    /// server-aggregated tally so the badge appears and the reaction can be
    /// withdrawn immediately, before the next timeline reload re-derives the
    /// authoritative aggregate. A first reaction for `key` adds the owner's
    /// distinct-sender contribution to `count` and sets `me`; the returned
    /// reaction event id is recorded for later withdrawal.
    pub(super) fn apply_local_reaction(
        &mut self,
        target_event_id: &str,
        key: &str,
        reaction_event_id: String,
    ) {
        let Some(room) = self.selected_room().cloned() else {
            return;
        };
        let room_key = RoomKey::from(&room);
        // A fourth tier beyond `own_user_id_for`, available only here: if this
        // reaction's own WS echo already landed, its raw `m.reaction` row names
        // the sender, and that sender is us. Without it, a client that cannot
        // otherwise name itself has no way to see that the echo already counted
        // this reaction, and counts it a second time (#220).
        let own_user_id = self
            .own_user_id_for(&room)
            .or_else(|| self.own_user_id_from_reaction_rows(&room_key, &[&reaction_event_id]));
        let Some(events) = self.messages.events.get_mut(&room_key) else {
            return;
        };
        let Some(event) = events.iter_mut().find(|e| e.event_id == target_event_id) else {
            return;
        };
        let reactions = event.reactions.get_or_insert_with(HashMap::new);
        let tally = reactions
            .entry(key.to_owned())
            .or_insert_with(crate::api::ReactionTally::default);
        record_reaction(
            tally,
            own_user_id.as_deref(),
            Some(&reaction_event_id),
            true,
        );
    }

    /// Fold a live `m.reaction` from any sender into the target message's
    /// server-aggregated tally.
    ///
    /// `append_live_event` has had a merge branch for edits since M8 — an
    /// `m.replace` frame patches the body of the event it edits — but none for
    /// reactions. A reaction frame appended a raw `m.reaction` row that
    /// `should_show_event` filters out, while the badge renders from the
    /// *target's* `reactions` aggregate, which nothing updated. The badge
    /// therefore only appeared after a room switch refetched the timeline.
    ///
    /// Idempotent: a sender already in the tally is a no-op, so a duplicate or
    /// echoed frame cannot double-count.
    pub(super) fn apply_remote_reaction(
        &mut self,
        room: &RoomKey,
        reaction_event_id: &str,
        target_event_id: &str,
        key: &str,
        sender: &str,
        own_user_id: Option<&str>,
    ) {
        // Tier 4, resolved before the mutable borrow below: when the caller's
        // three tiers came up empty, a reaction already recorded as ours on
        // this tally names us through its own raw row. Without it, our own
        // reaction arriving from a second device reads as a new sender and the
        // badge counts one person twice (#220 review, pass 4).
        let resolved_own = own_user_id.map(str::to_owned).or_else(|| {
            let mine = self.own_reaction_ids(room, target_event_id, key);
            let mine: Vec<&str> = mine.iter().map(String::as_str).collect();
            self.own_user_id_from_reaction_rows(room, &mine)
        });
        let own_user_id = resolved_own.as_deref();
        let Some(events) = self.messages.events.get_mut(room) else {
            return;
        };
        let Some(event) = events
            .iter_mut()
            .find(|item| item.event_id == target_event_id)
        else {
            return;
        };
        let reactions = event.reactions.get_or_insert_with(HashMap::new);
        let tally = reactions
            .entry(key.to_owned())
            .or_insert_with(crate::api::ReactionTally::default);
        // Our own reaction echoing back. Keyed on the reaction event id rather
        // than on the sender, because `own_user_id` is only known once the
        // account's user id has been resolved — an older server without
        // `RoomDto.account_user_id`, or a session where the user has not yet
        // sent a plain message, leaves it `None`. `apply_local_reaction` already
        // counted this and recorded the id, so a sender-only check would miss
        // the echo in exactly those cases and count one reaction twice.
        if tally.my_event_ids.iter().any(|id| id == reaction_event_id) {
            return;
        }
        record_reaction(
            tally,
            Some(sender),
            Some(reaction_event_id),
            own_user_id == Some(sender),
        );
    }

    pub(crate) fn start_react_to_selected_message(&mut self) {
        let Some(event) = self.selected_message_event() else {
            self.status = Status::from("select a displayed message before reacting".to_owned());
            return;
        };
        if event.event_id.starts_with("local-echo:") {
            self.status = Status::from(super::PENDING_ECHO_MSG.to_owned());
            return;
        }
        let event_id = event.event_id.clone();
        // Settle the room's draft before the buffer is cleared for the reaction,
        // so returning to compose restores it instead of tombstoning it (M12).
        self.flush_pending_draft_now();
        self.input.buffer.clear();
        self.input.cursor = 0;
        self.input.react_tab = None;
        self.mode = Mode::Reacting {
            event_id: event_id.clone(),
        };
        self.status = Status::EventAction {
            debug: format!("react to {} - type emoji, Enter to send", event_id),
            redacted: "react to message - type emoji, Enter to send",
        };
    }

    pub(crate) async fn send_react(&mut self, event_id: &str, key: &str) {
        if key.is_empty() {
            self.status = Status::from("reaction key cannot be empty".to_owned());
            return;
        }
        let Some(room) = self.selected_room().cloned() else {
            self.status = Status::from("no room selected".to_owned());
            return;
        };
        let result = self
            .client
            .react(room.account_id, &room.room_id, event_id, key)
            .await;
        self.status = match result {
            Ok(r) => {
                self.apply_local_reaction(event_id, key, r.event_id.clone());
                Status::EventAction {
                    debug: format!("reacted: {}", r.event_id),
                    redacted: "reacted",
                }
            }
            Err(err) => Status::Info(format!("react failed: {err}")),
        };
    }

    pub(crate) fn update_react_status(&mut self, event_id: &str) {
        if self.input.buffer.is_empty() {
            self.status = Status::EventAction {
                debug: format!("react to {event_id} - type emoji name, Enter to send"),
                redacted: "react to message - type emoji name, Enter to send",
            };
            return;
        }
        let matches = emoji_matches(&self.input.buffer);
        self.status = Status::Info(match matches.as_slice() {
            [] => format!("no emoji matches '{}'", self.input.buffer),
            [single] => format!("{} {} - Enter to send", single.as_str(), single.name()),
            _ => {
                let shown: Vec<_> = matches
                    .iter()
                    .take(5)
                    .map(|e| format!("{} {}", e.as_str(), e.name()))
                    .collect();
                let more = matches.len().saturating_sub(5);
                if more == 0 {
                    format!("matches: {} - Tab/Shift-Tab to select", shown.join(", "))
                } else {
                    format!(
                        "matches: {}, +{more} more - Tab/Shift-Tab to select",
                        shown.join(", ")
                    )
                }
            }
        });
    }

    pub(crate) fn complete_react_input(&mut self, reverse: bool) {
        let matches = emoji_matches(&self.input.buffer);
        if matches.len() < 2 {
            return;
        }
        let next = self
            .input
            .react_tab
            .map(|index| cycle_index(index, matches.len(), reverse))
            .unwrap_or_else(|| if reverse { matches.len() - 1 } else { 0 });
        self.input.react_tab = Some(next);
        self.status = Status::Info(emoji_cycle_status(next, &matches));
    }
}

pub(crate) fn unreact_selection_status(choices: &[OwnReaction], selected: usize) -> Status {
    let choice = &choices[selected];
    Status::Info(format!(
        "[{}/{}] withdraw {} - Tab/Shift-Tab to cycle, Enter to confirm",
        selected + 1,
        choices.len(),
        choice.key
    ))
}

pub(crate) fn emoji_matches(query: &str) -> Vec<&'static emojis::Emoji> {
    if query.is_empty() {
        return vec![];
    }
    let q = query.to_ascii_lowercase();
    emojis::iter()
        .filter(|emoji| emoji.name().to_ascii_lowercase().contains(q.as_str()))
        .collect()
}

/// Build `event_id -> [(key, count)]` reaction badges from the server-aggregated
/// `reactions` map on each event. The collapsed timeline no longer carries raw
/// `m.reaction` rows (M8), so the tally is read from the message it annotates
/// rather than re-counted from reaction events. A redacted message shows no
/// badges; reactions with a zero count are dropped.
pub(crate) fn collect_reactions(
    events: &[crate::api::EventDto],
) -> HashMap<String, Vec<(String, usize)>> {
    let mut map: HashMap<String, Vec<(String, usize)>> = HashMap::new();
    for event in events {
        if event.redacted {
            continue;
        }
        let Some(reactions) = &event.reactions else {
            continue;
        };
        let mut pairs: Vec<(String, usize)> = reactions
            .iter()
            .filter(|(_, tally)| tally.count > 0)
            .map(|(key, tally)| (key.clone(), tally.count as usize))
            .collect();
        if pairs.is_empty() {
            continue;
        }
        pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        map.insert(event.event_id.clone(), pairs);
    }
    map
}
