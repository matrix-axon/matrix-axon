//! M19 room membership and moderation actions.

use std::collections::HashSet;
use std::fmt;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::api::RoomDto;
use crate::command::UserReasonCommand;

use super::{App, Mode, RoomKey, RoomTargetResolution, Status};

/// Room-list checks after a leave axon answered with a `504`, each counted
/// from the one before's scheduled time: 30 s, 90 s, 210 s and 450 s after it. The live bus
/// carries no membership change (#103), so nothing else would show the room
/// going. On prod a leave of a 36,000-member space took Synapse 194 s and its
/// retry 328 s (#743). The web client uses the same schedule
/// (`PENDING_MEMBERSHIP_REFRESH_DELAYS_MS`). A pending forget changes nothing
/// the list shows, so it only waits the schedule out.
pub(crate) const PENDING_MEMBERSHIP_CHECK_DELAYS: [Duration; 4] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(240),
];

/// How long the last check's room-list read gets to land before a leave still
/// listed is reported as possibly failed.
const PENDING_MEMBERSHIP_FINAL_GRACE: Duration = Duration::from_secs(30);

/// A leave or forget axon timed out on, which the homeserver may still finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingMembership {
    pub(crate) room_title: String,
    /// When the `504` arrived. A room-list read started earlier cannot speak
    /// for the leave.
    pub(crate) since: Instant,
    /// Checks run so far. Equal to the number of delays once the last has run
    /// and only the grace period is left.
    pub(crate) step: usize,
    pub(crate) next_check: Instant,
    /// Whether the first check has run. Before it a repeat of the same verb is
    /// not sent: the first may still be running, and on prod the retry cost
    /// Synapse more than the original. After it a deliberate retry goes out,
    /// since the `504` may have come from a proxy before axon saw the call.
    pub(crate) retryable: bool,
    /// Whether a room-list read that could speak for this room has listed it
    /// since the `504`. Only then can the window's end say it is still listed.
    pub(crate) seen_listed: bool,
}

/// What a room-list read covered, for judging pending leaves by it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RoomListRead {
    pub(crate) started: Instant,
    /// The account it was read for; `None` for all of them.
    pub(crate) account_filter: Option<Uuid>,
    /// Axon listed no rooms at all.
    pub(crate) empty: bool,
}

/// Why a room action failed. `timed_out` marks a `504`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoomActionFailure {
    pub(crate) message: String,
    pub(crate) timed_out: bool,
}

impl fmt::Display for RoomActionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RoomActionKind {
    Leave,
    Forget,
    Invite,
    Kick,
    Ban,
    Unban,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingRoomAction {
    pub(crate) kind: RoomActionKind,
    pub(crate) key: RoomKey,
    pub(crate) room_title: String,
    pub(crate) user_id: Option<String>,
    pub(crate) reason: Option<String>,
}

#[derive(Debug)]
pub(crate) struct RoomActionOutcome {
    pub(crate) action: PendingRoomAction,
    pub(crate) result: Result<(), RoomActionFailure>,
}

impl RoomActionKind {
    fn present_participle(self) -> &'static str {
        match self {
            Self::Leave => "leaving",
            Self::Forget => "forgetting",
            Self::Invite => "inviting",
            Self::Kick => "kicking",
            Self::Ban => "banning",
            Self::Unban => "unbanning",
        }
    }

    fn past_tense(self) -> &'static str {
        match self {
            Self::Leave => "left",
            Self::Forget => "forgot",
            Self::Invite => "invited",
            Self::Kick => "kicked",
            Self::Ban => "banned",
            Self::Unban => "unbanned",
        }
    }

    fn command_name(self) -> &'static str {
        match self {
            Self::Leave => "leave",
            Self::Forget => "forget",
            Self::Invite => "invite",
            Self::Kick => "kick",
            Self::Ban => "ban",
            Self::Unban => "unban",
        }
    }

    fn needs_confirmation(self) -> bool {
        !matches!(self, Self::Invite)
    }

    fn removes_room_from_list(self) -> bool {
        matches!(self, Self::Leave | Self::Forget)
    }
}

impl App {
    pub(crate) fn start_leave_room(&mut self) {
        let Some(action) = self.action_for_selected_room(RoomActionKind::Leave, None, None) else {
            return;
        };
        self.request_or_perform_room_action(action);
    }

    pub(crate) fn start_forget_room(&mut self, target: Option<&str>) {
        let room = match target.filter(|target| !target.trim().is_empty()) {
            Some(target) => match self.resolve_room_target(target) {
                RoomTargetResolution::Match(index) => self.rooms.rooms.get(index).cloned(),
                RoomTargetResolution::Ambiguous(options) => {
                    self.status =
                        Status::Info(format!("room name is ambiguous: {}", options.join(", ")));
                    return;
                }
                RoomTargetResolution::Missing => {
                    self.status = Status::from(format!("room not found: {target}"));
                    return;
                }
            },
            None => self.selected_room().cloned(),
        };
        let Some(room) = room else {
            self.status = Status::from("select a room before using /forget".to_owned());
            return;
        };
        let action = action_for_room(&room, RoomActionKind::Forget, None, None);
        self.request_or_perform_room_action(action);
    }

    pub(crate) fn start_invite_user(&mut self, user_id: &str) {
        let Some(action) =
            self.action_for_selected_room(RoomActionKind::Invite, Some(user_id), None)
        else {
            return;
        };
        self.request_or_perform_room_action(action);
    }

    pub(crate) fn start_kick_user(&mut self, input: UserReasonCommand) {
        self.start_moderation_action(RoomActionKind::Kick, input);
    }

    pub(crate) fn start_ban_user(&mut self, input: UserReasonCommand) {
        self.start_moderation_action(RoomActionKind::Ban, input);
    }

    pub(crate) fn start_unban_user(&mut self, input: UserReasonCommand) {
        self.start_moderation_action(RoomActionKind::Unban, input);
    }

    fn start_moderation_action(&mut self, kind: RoomActionKind, input: UserReasonCommand) {
        let Some(action) =
            self.action_for_selected_room(kind, Some(&input.user_id), input.reason.as_deref())
        else {
            return;
        };
        self.request_or_perform_room_action(action);
    }

    fn action_for_selected_room(
        &mut self,
        kind: RoomActionKind,
        user_id: Option<&str>,
        reason: Option<&str>,
    ) -> Option<PendingRoomAction> {
        let Some(room) = self.selected_room().cloned() else {
            self.status = Status::from(format!(
                "select a room before using /{}",
                kind.command_name()
            ));
            return None;
        };
        Some(action_for_room(&room, kind, user_id, reason))
    }

    fn request_or_perform_room_action(&mut self, action: PendingRoomAction) {
        let held_back = |kind: RoomActionKind| {
            self.pending_memberships
                .get(&(action.key.clone(), kind))
                .is_some_and(|pending| !pending.retryable)
        };
        if held_back(action.kind) {
            self.status = Status::from(pending_status(action.kind, &action.room_title));
            return;
        }
        // Axon refuses a forget until the room is left, so one sent while the
        // leave is still running can only fail.
        if action.kind == RoomActionKind::Forget && held_back(RoomActionKind::Leave) {
            self.status = Status::from(format!(
                "your homeserver hasn't finished leaving {} yet, and a room can only be forgotten once it is left",
                action.room_title
            ));
            return;
        }
        if self.room_action_busy {
            self.status = Status::from("a room action is already in progress".to_owned());
            return;
        }
        if action.kind.needs_confirmation() {
            self.clear_input_buffer();
            self.status = Status::from(confirmation_status(&action));
            self.mode = Mode::ConfirmRoomAction { action };
        } else {
            self.perform_room_action(action);
        }
    }

    pub(crate) fn cancel_room_action_confirmation(&mut self) {
        self.mode = Mode::Compose;
        self.status = Status::Info("room action canceled".to_owned());
    }

    pub(crate) fn perform_room_action(&mut self, action: PendingRoomAction) {
        self.clear_input_buffer();
        self.mode = Mode::Compose;
        let Some(tx) = self.room_action_tx.clone() else {
            return;
        };
        let status = progress_status(&action);
        self.status = Status::from(status);
        self.room_action_busy = true;
        if matches!(action.kind, RoomActionKind::Leave | RoomActionKind::Forget) {
            self.stop_typing_for_room(&action.key);
        }
        let client = self.client.clone();
        let task_action = action.clone();
        tokio::spawn(async move {
            let account_id = task_action.key.account_id;
            let room_id = task_action.key.room_id.as_str();
            let result = match task_action.kind {
                RoomActionKind::Leave => client.leave_room(account_id, room_id).await,
                RoomActionKind::Forget => client.forget_room(account_id, room_id).await,
                RoomActionKind::Invite => {
                    client
                        .invite_user(
                            account_id,
                            room_id,
                            task_action.user_id.as_deref().unwrap_or_default(),
                        )
                        .await
                }
                RoomActionKind::Kick => {
                    client
                        .kick_user(
                            account_id,
                            room_id,
                            task_action.user_id.as_deref().unwrap_or_default(),
                            task_action.reason.as_deref(),
                        )
                        .await
                }
                RoomActionKind::Ban => {
                    client
                        .ban_user(
                            account_id,
                            room_id,
                            task_action.user_id.as_deref().unwrap_or_default(),
                            task_action.reason.as_deref(),
                        )
                        .await
                }
                RoomActionKind::Unban => {
                    client
                        .unban_user(
                            account_id,
                            room_id,
                            task_action.user_id.as_deref().unwrap_or_default(),
                            task_action.reason.as_deref(),
                        )
                        .await
                }
            }
            .map_err(|err| RoomActionFailure {
                timed_out: err.is_gateway_timeout(),
                message: err.to_string(),
            });
            let _ = tx.send(RoomActionOutcome {
                action: task_action,
                result,
            });
        });
    }

    pub(crate) async fn handle_room_action_outcome(&mut self, outcome: RoomActionOutcome) {
        self.room_action_busy = false;
        match outcome.result {
            Ok(()) => {
                if outcome.action.kind.removes_room_from_list() {
                    // An answer settles whatever an earlier timeout left
                    // pending: a successful forget proves the leave happened.
                    self.end_pending_memberships(&outcome.action.key);
                }
                let departed_selected =
                    self.selected_room().map(RoomKey::from) == Some(outcome.action.key.clone());
                let refresh_warning = match self.client.list_rooms(self.account_filter).await {
                    Ok(rooms) => {
                        self.apply_room_refresh(rooms);
                        None
                    }
                    Err(err) => Some(err.to_string()),
                };
                if outcome.action.kind.removes_room_from_list() && departed_selected {
                    self.sync_draft_on_room_change();
                    if self.selected_room().is_some() {
                        self.load_selected_timeline().await;
                    }
                }
                if matches!(
                    outcome.action.kind,
                    RoomActionKind::Invite
                        | RoomActionKind::Kick
                        | RoomActionKind::Ban
                        | RoomActionKind::Unban
                ) {
                    self.spawn_members_refresh(outcome.action.key.clone());
                }
                self.status = Status::from(match refresh_warning {
                    Some(err) => format!(
                        "{}; room refresh failed: {err}",
                        success_status(&outcome.action)
                    ),
                    None => success_status(&outcome.action),
                });
            }
            Err(failure) if failure.timed_out && outcome.action.kind.removes_room_from_list() => {
                // Axon stopped waiting; the homeserver has not refused.
                // Inviting a retry here is what doubled the cost on prod.
                self.begin_pending_membership(&outcome.action, Instant::now());
                self.status = Status::from(pending_status(
                    outcome.action.kind,
                    &outcome.action.room_title,
                ));
            }
            Err(err) => {
                self.status = Status::from(format!(
                    "/{} failed in {}: {err}",
                    outcome.action.kind.command_name(),
                    outcome.action.room_title
                ));
            }
        }
    }
}

impl App {
    fn begin_pending_membership(&mut self, action: &PendingRoomAction, now: Instant) {
        self.pending_memberships.insert(
            (action.key.clone(), action.kind),
            PendingMembership {
                room_title: action.room_title.clone(),
                since: now,
                step: 0,
                next_check: now + PENDING_MEMBERSHIP_CHECK_DELAYS[0],
                retryable: false,
                seen_listed: false,
            },
        );
    }

    fn end_pending_memberships(&mut self, key: &RoomKey) {
        self.pending_memberships
            .retain(|(pending_key, _), _| pending_key != key);
    }

    /// Run the checks that are due: a pending leave asks for a room-list read,
    /// which [`Self::settle_pending_leaves`] then judges; a pending forget only
    /// counts the schedule down. Called from the main loop's tick.
    pub(crate) fn poll_pending_memberships(&mut self, now: Instant) {
        let mut refresh = false;
        let mut expired = Vec::new();
        for ((key, kind), pending) in &mut self.pending_memberships {
            if now < pending.next_check {
                continue;
            }
            if pending.step >= PENDING_MEMBERSHIP_CHECK_DELAYS.len() {
                expired.push((key.clone(), *kind));
                continue;
            }
            pending.step += 1;
            pending.retryable = true;
            // From the scheduled time, not the tick that noticed it, so the
            // checks land 30/90/210/450 s after the `504`.
            pending.next_check += PENDING_MEMBERSHIP_CHECK_DELAYS
                .get(pending.step)
                .copied()
                .unwrap_or(PENDING_MEMBERSHIP_FINAL_GRACE);
            refresh |= *kind == RoomActionKind::Leave;
        }
        for (key, kind) in expired {
            let Some(pending) = self.pending_memberships.remove(&(key, kind)) else {
                continue;
            };
            // Unless a read that covered the room listed it, the window ran
            // out without evidence either way (its account filtered out or
            // logged out, or every read empty), so say nothing.
            if kind == RoomActionKind::Leave && pending.seen_listed && !self.is_mid_command() {
                self.status = Status::from(format!(
                    "{} is still listed; the leave may not have gone through, so /leave again to retry",
                    pending.room_title
                ));
            }
        }
        if refresh {
            self.request_rooms_refresh();
        }
    }

    /// End each pending leave whose room `read` no longer lists (ADR 0037
    /// drops left rooms). Only a read that covered the room counts: one for
    /// another account (judged by the filter it was read with, not the one in
    /// force now), one for a logged-out account, one started before the
    /// `504`, or one that listed no rooms at all says nothing about the leave.
    pub(super) fn settle_pending_leaves(&mut self, read: RoomListRead) {
        if self.pending_memberships.is_empty() || read.empty {
            return;
        }
        let listed: HashSet<RoomKey> = self.rooms.rooms.iter().map(RoomKey::from).collect();
        let mut settled = Vec::new();
        for ((key, kind), pending) in &mut self.pending_memberships {
            let covers = *kind == RoomActionKind::Leave
                && read.started >= pending.since
                && read
                    .account_filter
                    .is_none_or(|account_id| account_id == key.account_id)
                && !self.accounts.inactive_ids.contains(&key.account_id);
            if !covers {
                continue;
            }
            if listed.contains(key) {
                pending.seen_listed = true;
            } else {
                settled.push(((key.clone(), *kind), pending.room_title.clone()));
            }
        }
        for (entry, room_title) in settled {
            self.pending_memberships.remove(&entry);
            if !self.is_mid_command() {
                self.status = Status::from(format!("left {room_title}"));
            }
        }
    }
}

fn pending_status(kind: RoomActionKind, room_title: &str) -> String {
    match kind {
        RoomActionKind::Forget => format!(
            "your homeserver hasn't finished forgetting {room_title} yet; it will finish on its own, so there's no need to forget it again"
        ),
        _ => format!(
            "your homeserver hasn't finished leaving {room_title} yet; it will drop off the list once it has, so there's no need to leave again"
        ),
    }
}

fn action_for_room(
    room: &RoomDto,
    kind: RoomActionKind,
    user_id: Option<&str>,
    reason: Option<&str>,
) -> PendingRoomAction {
    PendingRoomAction {
        kind,
        key: RoomKey::from(room),
        room_title: room.title().to_owned(),
        user_id: user_id.map(str::to_owned),
        reason: reason
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .map(str::to_owned),
    }
}

fn confirmation_status(action: &PendingRoomAction) -> String {
    match action.kind {
        RoomActionKind::Leave => format!("Leave {}? [y/N]", action.room_title),
        RoomActionKind::Forget => format!("Forget {}? [y/N]", action.room_title),
        RoomActionKind::Kick => format!(
            "Kick {} from {}? [y/N]",
            action.user_id.as_deref().unwrap_or("user"),
            action.room_title
        ),
        RoomActionKind::Ban => format!(
            "Ban {} from {}? [y/N]",
            action.user_id.as_deref().unwrap_or("user"),
            action.room_title
        ),
        RoomActionKind::Unban => format!(
            "Unban {} from {}? [y/N]",
            action.user_id.as_deref().unwrap_or("user"),
            action.room_title
        ),
        RoomActionKind::Invite => progress_status(action),
    }
}

fn progress_status(action: &PendingRoomAction) -> String {
    match action.user_id.as_deref() {
        Some(user_id) => format!(
            "{} {user_id} in {}...",
            action.kind.present_participle(),
            action.room_title
        ),
        None => format!(
            "{} {}...",
            action.kind.present_participle(),
            action.room_title
        ),
    }
}

fn success_status(action: &PendingRoomAction) -> String {
    match action.user_id.as_deref() {
        Some(user_id) => format!(
            "{} {user_id} in {}",
            action.kind.past_tense(),
            action.room_title
        ),
        None => format!("{} {}", action.kind.past_tense(), action.room_title),
    }
}
