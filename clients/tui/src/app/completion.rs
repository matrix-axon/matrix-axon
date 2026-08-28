use std::collections::HashMap;
use std::path::PathBuf;

use crate::api::RoomDto;
use crate::command::{self, SlashCommand, SLASH_COMMANDS};

use super::{
    cycle_index, emoji_matches, rooms::account_localpart, App, RoomKey, RoomTargetResolution,
    Status,
};

const FILTER_ARGUMENTS: &[&str] = &[
    "all",
    "dms",
    "dm",
    "people",
    "groups",
    "group",
    "rooms",
    "unread",
    "fav",
    "favs",
    "favorites",
];

impl App {
    pub(crate) fn complete_input(&mut self) {
        self.complete_input_in_direction(false);
    }

    pub(crate) fn complete_input_reverse(&mut self) {
        self.complete_input_in_direction(true);
    }

    fn complete_input_in_direction(&mut self, reverse: bool) {
        if self.complete_react_command_input(reverse) {
            return;
        }
        if self.complete_logout_command_input(reverse) {
            return;
        }
        if self.complete_recover_command_input(reverse) {
            return;
        }
        if self.complete_backup_command_input(reverse) {
            return;
        }
        if self.complete_delete_command_input(reverse) {
            return;
        }
        if self.complete_account_command_input(reverse) {
            return;
        }
        if self.complete_verify_command_input(reverse) {
            return;
        }
        if self.complete_room_action_user_command_input(reverse) {
            return;
        }
        if self.complete_filter_command_input(reverse) {
            return;
        }
        if self.complete_send_command_input(reverse) {
            return;
        }
        if self.complete_command_input() {
            return;
        }
        self.complete_room_input(reverse);
    }

    pub(crate) fn complete_filter_command_input(&mut self, reverse: bool) -> bool {
        let Some(target) = filter_target_prefix(&self.input.buffer) else {
            return false;
        };
        let query = self
            .input
            .filter_command_completion
            .as_ref()
            .map(|(query, _)| query.clone())
            .unwrap_or_else(|| target.to_owned());
        let candidates = filter_argument_candidates(&query);
        if candidates.is_empty() {
            self.input.filter_command_completion = None;
            return true;
        }

        let selected = if let Some((_, current)) = self.input.filter_command_completion.as_ref() {
            cycle_index(*current, candidates.len(), reverse)
        } else if reverse {
            candidates.len() - 1
        } else {
            0
        };
        let argument = candidates[selected];
        self.input.buffer = format!("/filter {argument}");
        self.move_cursor_to_end();
        self.input.filter_command_completion = Some((query, selected));
        self.status = Status::Info(format!(
            "[{}/{}] {} - Tab/Shift-Tab to cycle, Enter to filter",
            selected + 1,
            candidates.len(),
            argument
        ));
        true
    }

    /// `/send <path> [caption]` filename completion: lists filesystem entries
    /// matching the partial path token, following the same prefix-advance /
    /// full-cycle shape as `complete_room_input`. Declines to act (returns
    /// `false`) once the caption text has started, so Tab in that position
    /// falls through to the generic completers instead.
    pub(crate) fn complete_send_command_input(&mut self, reverse: bool) -> bool {
        if let Some((query, current)) = self.input.send_command_completion.clone() {
            let candidates = send_path_candidates(&query);
            if candidates.is_empty() {
                self.input.send_command_completion = None;
                return true;
            }
            let selected = cycle_index(current, candidates.len(), reverse);
            self.apply_send_path_selection(&query, selected, &candidates);
            return true;
        }

        let Some(target) = send_target_prefix(&self.input.buffer) else {
            return false;
        };
        if send_path_completion_token(target).is_none() {
            // Inside /send, but past the path token (caption text has
            // started) — consume the Tab as a no-op rather than falling
            // through to unrelated completers.
            return true;
        }
        let (partial, _caption) = command::parse_leading_path_token(target);
        let candidates = send_path_candidates(&partial);
        match candidates.as_slice() {
            [] => {
                self.status = Status::Info(format!("no path matches: {partial}"));
            }
            [only] => {
                self.input.buffer = format!("/send {}", quote_path_for_send(only));
                self.move_cursor_to_end();
                self.status = Status::from(format!("completed path: {only}"));
            }
            _ => {
                let common = longest_common_prefix(&candidates);
                let completed = common
                    .as_deref()
                    .filter(|prefix| prefix.len() > partial.len() && prefix.starts_with(&partial))
                    .unwrap_or(&partial);
                if completed != partial {
                    self.input.buffer = format!("/send {}", quote_path_for_send(completed));
                    self.move_cursor_to_end();
                }
                if completed == partial {
                    let selected = if reverse { candidates.len() - 1 } else { 0 };
                    self.apply_send_path_selection(&partial, selected, &candidates);
                    return true;
                }
                let shown: Vec<&String> = candidates.iter().take(5).collect();
                let more = candidates.len().saturating_sub(5);
                let shown_joined = shown
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                self.status = Status::Info(if more == 0 {
                    format!("path completions: {shown_joined}")
                } else {
                    format!("path completions: {shown_joined}, +{more} more")
                });
            }
        }
        true
    }

    fn apply_send_path_selection(&mut self, query: &str, selected: usize, candidates: &[String]) {
        let candidate = &candidates[selected];
        self.input.buffer = format!("/send {}", quote_path_for_send(candidate));
        self.move_cursor_to_end();
        self.input.send_command_completion = Some((query.to_owned(), selected));
        self.status = Status::Info(format!(
            "[{}/{}] {} - Tab/Shift-Tab to cycle, Enter to send",
            selected + 1,
            candidates.len(),
            candidate,
        ));
    }

    pub(crate) fn complete_recover_command_input(&mut self, reverse: bool) -> bool {
        let Some(target) = command_target_prefix(&self.input.buffer, "/recover") else {
            return false;
        };
        let query = self
            .input
            .recover_command_completion
            .as_ref()
            .map(|(query, _)| query.clone())
            .unwrap_or_else(|| target.to_owned());
        let candidates = self.active_recover_candidates(&query);
        if candidates.is_empty() {
            self.input.recover_command_completion = None;
            self.status = Status::Info(if query.is_empty() {
                "no active accounts".to_owned()
            } else {
                format!("no active account matches: {query}")
            });
            return true;
        }

        let selected = if let Some((_, current)) = self.input.recover_command_completion.as_ref() {
            cycle_index(*current, candidates.len(), reverse)
        } else if reverse {
            candidates.len() - 1
        } else {
            0
        };
        let user_id = &candidates[selected];
        self.input.buffer = format!("/recover {user_id}");
        self.move_cursor_to_end();
        self.input.recover_command_completion = Some((query, selected));
        self.status = Status::Info(format!(
            "[{}/{}] {} - Tab/Shift-Tab to cycle, Enter to recover",
            selected + 1,
            candidates.len(),
            user_id
        ));
        true
    }

    pub(crate) fn complete_backup_command_input(&mut self, reverse: bool) -> bool {
        if let Some(target) = command_target_prefix(&self.input.buffer, "/backup enable") {
            let target = target.to_owned();
            return self.complete_backup_enable_account(&target, reverse);
        }
        let Some(target) = command_target_prefix(&self.input.buffer, "/backup") else {
            return false;
        };
        if target.is_empty()
            || ("enable".starts_with(target) && !target.chars().any(char::is_whitespace))
        {
            self.input.buffer = "/backup enable".to_owned();
            self.move_cursor_to_end();
            self.input.backup_command_completion = None;
            self.status = Status::Info("completed subcommand: /backup enable".to_owned());
            return true;
        }
        false
    }

    fn complete_backup_enable_account(&mut self, target: &str, reverse: bool) -> bool {
        let query = self
            .input
            .backup_command_completion
            .as_ref()
            .map(|(query, _)| query.clone())
            .unwrap_or_else(|| target.to_owned());
        let candidates = self.active_recover_candidates(&query);
        if candidates.is_empty() {
            self.input.backup_command_completion = None;
            self.status = Status::Info(if query.is_empty() {
                "no active accounts".to_owned()
            } else {
                format!("no active account matches: {query}")
            });
            return true;
        }

        let selected = if let Some((_, current)) = self.input.backup_command_completion.as_ref() {
            cycle_index(*current, candidates.len(), reverse)
        } else if reverse {
            candidates.len() - 1
        } else {
            0
        };
        let user_id = &candidates[selected];
        self.input.buffer = format!("/backup enable {user_id}");
        self.move_cursor_to_end();
        self.input.backup_command_completion = Some((query, selected));
        self.status = Status::Info(format!(
            "[{}/{}] {} - Tab/Shift-Tab to cycle, Enter to enable backup",
            selected + 1,
            candidates.len(),
            user_id
        ));
        true
    }

    pub(crate) fn complete_logout_command_input(&mut self, reverse: bool) -> bool {
        let Some(target) = command_target_prefix(&self.input.buffer, "/logout") else {
            return false;
        };
        let query = self
            .input
            .logout_command_completion
            .as_ref()
            .map(|(query, _)| query.clone())
            .unwrap_or_else(|| target.to_owned());
        let candidates = self.active_logout_candidates(&query);
        if candidates.is_empty() {
            self.input.logout_command_completion = None;
            self.status = Status::Info(if query.is_empty() {
                "no active accounts".to_owned()
            } else {
                format!("no active account matches: {query}")
            });
            return true;
        }

        let selected = if let Some((_, current)) = self.input.logout_command_completion.as_ref() {
            cycle_index(*current, candidates.len(), reverse)
        } else if reverse {
            candidates.len() - 1
        } else {
            0
        };
        let user_id = &candidates[selected];
        self.input.buffer = format!("/logout {user_id}");
        self.move_cursor_to_end();
        self.input.logout_command_completion = Some((query, selected));
        self.status = Status::Info(format!(
            "[{}/{}] {} - Tab/Shift-Tab to cycle, Enter to log out",
            selected + 1,
            candidates.len(),
            user_id
        ));
        true
    }

    pub(crate) fn complete_delete_command_input(&mut self, reverse: bool) -> bool {
        let Some(target) = command_target_prefix(&self.input.buffer, "/delete") else {
            return false;
        };
        let query = self
            .input
            .delete_command_completion
            .as_ref()
            .map(|(query, _)| query.clone())
            .unwrap_or_else(|| target.to_owned());
        let candidates = self.delete_candidates(&query);
        if candidates.is_empty() {
            self.input.delete_command_completion = None;
            self.status = Status::Info(if query.is_empty() {
                "no accounts".to_owned()
            } else {
                format!("no account matches: {query}")
            });
            return true;
        }

        let selected = if let Some((_, current)) = self.input.delete_command_completion.as_ref() {
            cycle_index(*current, candidates.len(), reverse)
        } else if reverse {
            candidates.len() - 1
        } else {
            0
        };
        let user_id = &candidates[selected];
        self.input.buffer = format!("/delete {user_id}");
        self.move_cursor_to_end();
        self.input.delete_command_completion = Some((query, selected));
        self.status = Status::Info(format!(
            "[{}/{}] {} - Tab/Shift-Tab to cycle, Enter to delete",
            selected + 1,
            candidates.len(),
            user_id
        ));
        true
    }

    pub(crate) fn complete_account_command_input(&mut self, reverse: bool) -> bool {
        let Some(target) = command_target_prefix(&self.input.buffer, "/account") else {
            return false;
        };
        let query = self
            .input
            .account_command_completion
            .as_ref()
            .map(|(query, _)| query.clone())
            .unwrap_or_else(|| target.to_owned());
        let candidates = self.account_completion_candidates(&query);
        if candidates.is_empty() {
            self.input.account_command_completion = None;
            self.status = Status::Info(if query.is_empty() {
                "no accounts".to_owned()
            } else {
                format!("no account matches: {query}")
            });
            return true;
        }
        let selected = if let Some((_, current)) = self.input.account_command_completion.as_ref() {
            cycle_index(*current, candidates.len(), reverse)
        } else if reverse {
            candidates.len() - 1
        } else {
            0
        };
        let user_id = &candidates[selected];
        self.input.buffer = format!("/account {user_id}");
        self.move_cursor_to_end();
        self.input.account_command_completion = Some((query, selected));
        self.status = Status::Info(format!(
            "[{}/{}] {} - Tab/Shift-Tab to cycle, Enter to select",
            selected + 1,
            candidates.len(),
            user_id
        ));
        true
    }

    fn account_completion_candidates(&self, target: &str) -> Vec<String> {
        let target_lower = target.to_lowercase();
        self.accounts
            .accounts
            .iter()
            .filter(|a| {
                if target.is_empty() {
                    return true;
                }
                a.user_id.to_lowercase().contains(&target_lower)
                    || account_localpart(&a.user_id)
                        .is_some_and(|local| local.to_lowercase().contains(&target_lower))
            })
            .map(|a| a.user_id.clone())
            .collect()
    }

    pub(crate) fn complete_verify_command_input(&mut self, reverse: bool) -> bool {
        let Some(target) = command_target_prefix(&self.input.buffer, "/verify") else {
            return false;
        };
        // Only complete users (cross-user verification, ADR 0040); a device id for
        // self-verification is pasted, not completed.
        let query = self
            .input
            .verify_command_completion
            .as_ref()
            .map(|(query, _)| query.clone())
            .unwrap_or_else(|| target.to_owned());
        let candidates = self.verify_completion_candidates(&query);
        if candidates.is_empty() {
            self.input.verify_command_completion = None;
            self.status = Status::Info(if query.trim().is_empty() {
                "no known users in this room yet".to_owned()
            } else {
                format!("no user matches: {query}")
            });
            return true;
        }
        let selected = if let Some((_, current)) = self.input.verify_command_completion.as_ref() {
            cycle_index(*current, candidates.len(), reverse)
        } else if reverse {
            candidates.len() - 1
        } else {
            0
        };
        let user_id = &candidates[selected];
        self.input.buffer = format!("/verify {user_id}");
        self.move_cursor_to_end();
        self.input.verify_command_completion = Some((query, selected));
        self.status = Status::Info(format!(
            "[{}/{}] {} - Tab/Shift-Tab to cycle, Enter to verify",
            selected + 1,
            candidates.len(),
            user_id
        ));
        true
    }

    /// Users known in the currently-selected room (from the per-room display-name
    /// map), matched by user id, localpart, or display name. The account's own user
    /// is excluded — verifying yourself is self-verification, not cross-user.
    fn verify_completion_candidates(&self, target: &str) -> Vec<String> {
        self.selected_room_user_candidates(target)
    }

    pub(crate) fn complete_room_action_user_command_input(&mut self, reverse: bool) -> bool {
        let Some((command, target)) = room_action_user_target_prefix(&self.input.buffer) else {
            return false;
        };
        let target = target.trim_start();
        if target.chars().any(char::is_whitespace) {
            return true;
        }
        let query = self
            .input
            .verify_command_completion
            .as_ref()
            .map(|(query, _)| query.clone())
            .unwrap_or_else(|| target.to_owned());
        let candidates = self.selected_room_user_candidates(&query);
        if candidates.is_empty() {
            self.input.verify_command_completion = None;
            self.status = Status::Info(if query.trim().is_empty() {
                "no known users in this room yet".to_owned()
            } else {
                format!("no user matches: {query}")
            });
            return true;
        }
        let selected = if let Some((_, current)) = self.input.verify_command_completion.as_ref() {
            cycle_index(*current, candidates.len(), reverse)
        } else if reverse {
            candidates.len() - 1
        } else {
            0
        };
        let user_id = &candidates[selected];
        self.input.buffer = format!("{command} {user_id}");
        self.move_cursor_to_end();
        self.input.verify_command_completion = Some((query, selected));
        self.status = Status::Info(format!(
            "[{}/{}] {} - Tab/Shift-Tab to cycle, Enter to run {}",
            selected + 1,
            candidates.len(),
            user_id,
            command
        ));
        true
    }

    fn selected_room_user_candidates(&self, target: &str) -> Vec<String> {
        let Some(room) = self.selected_room() else {
            return Vec::new();
        };
        let own_user = room.account_user_id.as_deref();
        let key = RoomKey::from(room);
        let Some(names) = self.rooms.display_names.get(&key) else {
            return Vec::new();
        };
        let query = target.trim().trim_start_matches('@').to_lowercase();
        let mut candidates: Vec<String> = names
            .iter()
            .filter(|(user_id, _)| Some(user_id.as_str()) != own_user)
            .filter(|(user_id, display)| {
                if query.is_empty() {
                    return true;
                }
                user_id.to_lowercase().contains(&query)
                    || account_localpart(user_id)
                        .is_some_and(|local| local.to_lowercase().contains(&query))
                    || display.to_lowercase().contains(&query)
            })
            .map(|(user_id, _)| user_id.clone())
            .collect();
        candidates.sort();
        candidates.dedup();
        candidates
    }

    pub(crate) fn complete_react_command_input(&mut self, reverse: bool) -> bool {
        let (query, next) =
            if let Some((query, current)) = self.input.react_command_completion.as_ref() {
                let matches = emoji_matches(query);
                let next = cycle_index(*current, matches.len(), reverse);
                (query.clone(), next)
            } else {
                let Some(query) = react_command_emoji_prefix(&self.input.buffer) else {
                    return false;
                };
                let matches = emoji_matches(query);
                let next = if reverse {
                    matches.len().saturating_sub(1)
                } else {
                    0
                };
                (query.to_owned(), next)
            };
        let matches = emoji_matches(&query);
        if matches.is_empty() {
            self.input.react_command_completion = None;
            self.status = Status::Info(format!("no emoji matches '{query}'"));
            return true;
        }
        let selected = next % matches.len();
        let emoji = matches[selected];
        self.input.buffer = format!("/react {}", emoji.as_str());
        self.move_cursor_to_end();
        self.input.react_command_completion = Some((query, selected));
        self.status = Status::Info(emoji_cycle_status(selected, &matches));
        true
    }

    pub(crate) fn complete_command_input(&mut self) -> bool {
        let Some(prefix) = slash_command_prefix(&self.input.buffer) else {
            return false;
        };
        let candidates = slash_command_candidates(prefix);
        match candidates.as_slice() {
            [] => {
                self.status = Status::from(format!("no command matches: /{prefix}"));
            }
            [command] => {
                self.input.buffer = if command.takes_argument {
                    format!("{} ", command.name)
                } else {
                    command.name.to_owned()
                };
                self.move_cursor_to_end();
                self.status = Status::from(format!("completed command: {}", command.name));
            }
            _ => {
                self.status = Status::Info(format!(
                    "command matches: {}",
                    candidates
                        .iter()
                        .map(|command| command.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        true
    }

    pub(crate) fn complete_room_input(&mut self, reverse: bool) {
        // If already cycling through same-name duplicates, continue cycling.
        if let Some((query, current)) = self.input.room_command_completion.clone() {
            let candidates = self.room_cycle_candidates(&query);
            if candidates.is_empty() {
                self.input.room_command_completion = None;
                return;
            }
            let selected = cycle_index(current, candidates.len(), reverse);
            self.apply_room_cycle_selection(&query, selected, &candidates);
            return;
        }

        let Some(target) = room_target_prefix(&self.input.buffer) else {
            return;
        };
        let target = target.to_owned();
        let candidates = self.room_completion_candidates(&target);
        match candidates.as_slice() {
            [] => {
                self.input.partial_room_completions = None;
                self.status = Status::Info(format!("no room matches: {target}"));
            }
            [completion] => {
                self.input.partial_room_completions = None;
                self.input.buffer = format!("/room {completion}");
                self.move_cursor_to_end();
                self.status = Status::from(format!("completed room: {completion}"));
            }
            _ => {
                let prefix_candidates = self.room_prefix_candidates(&target);
                let common_prefix = longest_common_prefix(&prefix_candidates);
                let completed = common_prefix
                    .as_deref()
                    .filter(|prefix| {
                        prefix.len() > target.len()
                            && prefix
                                .get(..target.len())
                                .is_some_and(|start| start.eq_ignore_ascii_case(&target))
                    })
                    .unwrap_or(&target);
                if completed != target {
                    self.input.buffer = format!("/room {completed}");
                    self.move_cursor_to_end();
                }

                // When all candidates collapse to the same display value and
                // prefix expansion can't help further, cycle with disambiguators
                // so the user can distinguish rooms that share a display name.
                let all_same = candidates.windows(2).all(|w| w[0] == w[1]);
                if all_same && completed == target {
                    let cycle_candidates = self.room_cycle_candidates(&target);
                    if cycle_candidates.len() > 1 {
                        let selected = if reverse {
                            cycle_candidates.len() - 1
                        } else {
                            0
                        };
                        self.apply_room_cycle_selection(&target, selected, &cycle_candidates);
                        return;
                    }
                }

                let shown_candidates = if completed != target {
                    &prefix_candidates
                } else {
                    &candidates
                };
                let displayed: Vec<_> = shown_candidates
                    .iter()
                    .take(5)
                    .map(|candidate| {
                        case_insensitive_suffix(candidate, completed)
                            .filter(|suffix| !suffix.is_empty())
                            .unwrap_or(candidate)
                            .to_owned()
                    })
                    .collect();
                let shown = displayed.join(", ");
                let more = candidates.len().saturating_sub(5);
                self.input.partial_room_completions = Some(displayed);
                self.status = Status::Info(if more == 0 {
                    format!("room completions: {shown}")
                } else {
                    format!("room completions: {shown}, +{more} more")
                });
            }
        }
    }

    /// Deduplicated view of visible rooms for completion: when the same Matrix
    /// room appears under multiple accounts (one per `account_id`), keep only
    /// the entry with the best display value (canonical alias > name > room_id).
    /// This prevents the same room from showing up as both "#scratch:example.com"
    /// and "scratch" because one account hasn't synced the alias state event yet.
    fn visible_rooms_for_completion(&self) -> Vec<&RoomDto> {
        let mut seen: HashMap<&str, usize> = HashMap::new();
        let mut result: Vec<&RoomDto> = Vec::new();
        for index in self.visible_room_indices() {
            let Some(room) = self.rooms.rooms.get(index) else {
                continue;
            };
            match seen.get(room.room_id.as_str()).copied() {
                None => {
                    seen.insert(&room.room_id, result.len());
                    result.push(room);
                }
                Some(idx) => {
                    if result[idx].canonical_alias.is_none() && room.canonical_alias.is_some() {
                        result[idx] = room;
                    }
                }
            }
        }
        result
    }

    fn room_cycle_candidates(&self, target: &str) -> Vec<(String, String, String)> {
        self.visible_rooms_for_completion()
            .into_iter()
            .filter(|room| room_matches_completion(room, target))
            .map(|room| {
                let display = room_completion_value(room);
                // Prefer alias as disambiguator (more readable), but if the alias
                // IS the display value fall back to the raw room ID.
                let disambig = if room.canonical_alias.as_deref() == Some(display.as_str()) {
                    room.room_id.clone()
                } else {
                    room.canonical_alias
                        .as_deref()
                        .unwrap_or(&room.room_id)
                        .to_owned()
                };
                (room.room_id.clone(), display, disambig)
            })
            .collect()
    }

    fn apply_room_cycle_selection(
        &mut self,
        query: &str,
        selected: usize,
        candidates: &[(String, String, String)],
    ) {
        let (room_id, display, disambig) = &candidates[selected];
        self.input.buffer = format!("/room {room_id}");
        self.move_cursor_to_end();
        self.input.partial_room_completions = None;
        self.input.room_command_completion = Some((query.to_owned(), selected));
        self.status = Status::Info(format!(
            "[{}/{}] {}  ·  {}  —  Tab/Shift-Tab to cycle, Enter to select",
            selected + 1,
            candidates.len(),
            display,
            disambig,
        ));
    }

    fn room_completion_candidates(&self, target: &str) -> Vec<String> {
        self.visible_rooms_for_completion()
            .into_iter()
            .filter(|room| room_matches_completion(room, target))
            .map(room_completion_value)
            .collect()
    }

    fn room_prefix_candidates(&self, target: &str) -> Vec<String> {
        self.visible_rooms_for_completion()
            .into_iter()
            .filter_map(|room| room_matching_prefix_value(room, target))
            .collect()
    }

    pub(super) fn resolve_room_target(&self, target: &str) -> RoomTargetResolution {
        self.resolve_room_target_in_account(target, self.active_account_filter())
    }

    pub(super) fn resolve_room_target_in_account(
        &self,
        target: &str,
        account_filter: Option<uuid::Uuid>,
    ) -> RoomTargetResolution {
        let target = target.trim();
        if let Ok(n) = target.parse::<usize>() {
            let visible = self.visible_room_indices_for_account(account_filter);
            return n
                .checked_sub(1)
                .and_then(|vis_pos| visible.get(vis_pos).copied())
                .map(RoomTargetResolution::Match)
                .unwrap_or(RoomTargetResolution::Missing);
        }
        let target_lower = target.to_lowercase();
        let exact = self.matching_room_indices_for_account(account_filter, |room| {
            room.room_id == target
                || room.canonical_alias.as_deref() == Some(target)
                || room.name.as_deref().map(str::to_lowercase).as_deref()
                    == Some(target_lower.as_str())
        });
        if let Some(resolution) = self.classify_room_matches(target, exact) {
            return resolution;
        }

        if let Some(alias) = room_alias_with_hash(target) {
            let matches = self.matching_room_indices_for_account(account_filter, |room| {
                room.canonical_alias.as_deref() == Some(alias.as_str())
            });
            if let Some(resolution) = self.classify_room_matches(target, matches) {
                return resolution;
            }
        }

        if let Some(target_local) = incomplete_matrix_room_name(target) {
            let local_matches = self.matching_room_indices_for_account(account_filter, |room| {
                room.canonical_alias
                    .as_deref()
                    .and_then(matrix_room_local_name)
                    .is_some_and(|local| local.eq_ignore_ascii_case(target_local))
                    || matrix_room_local_name(&room.room_id)
                        .is_some_and(|local| local.eq_ignore_ascii_case(target_local))
            });
            if let Some(resolution) = self.classify_room_matches(target, local_matches) {
                return resolution;
            }
        }

        let prefix_matches = self.matching_room_indices_for_account(account_filter, |room| {
            room_matches_completion(room, target)
        });
        self.classify_room_matches(target, prefix_matches)
            .unwrap_or(RoomTargetResolution::Missing)
    }

    fn matching_room_indices_for_account(
        &self,
        account_filter: Option<uuid::Uuid>,
        predicate: impl Fn(&RoomDto) -> bool,
    ) -> Vec<usize> {
        self.visible_room_indices_for_account(account_filter)
            .into_iter()
            .filter(|index| self.rooms.rooms.get(*index).is_some_and(&predicate))
            .collect()
    }

    fn visible_room_indices_for_account(&self, account_filter: Option<uuid::Uuid>) -> Vec<usize> {
        self.rooms
            .rooms
            .iter()
            .enumerate()
            .filter(|(_, room)| {
                account_filter.is_none_or(|account_id| room.account_id == account_id)
            })
            .filter(|(index, room)| {
                self.rooms.selected == Some(*index) || self.room_passes_filter(room)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn classify_room_matches(
        &self,
        target: &str,
        indices: Vec<usize>,
    ) -> Option<RoomTargetResolution> {
        match indices.as_slice() {
            [index] => Some(RoomTargetResolution::Match(*index)),
            [_, _, ..] => Some(RoomTargetResolution::Ambiguous(
                self.room_resolution_options(target, &indices),
            )),
            [] => None,
        }
    }

    fn room_resolution_options(&self, target: &str, indices: &[usize]) -> Vec<String> {
        indices
            .iter()
            .filter_map(|index| self.rooms.rooms.get(*index))
            .map(|room| {
                let value = room_matching_prefix_value(room, target)
                    .unwrap_or_else(|| room_completion_value(room));
                case_insensitive_suffix(&value, target)
                    .filter(|suffix| !suffix.is_empty())
                    .unwrap_or(&value)
                    .to_owned()
            })
            .collect()
    }
}

pub(super) fn emoji_cycle_status(selected: usize, matches: &[&'static emojis::Emoji]) -> String {
    let emoji = matches[selected];
    format!(
        "[{}/{}] {} {} - Tab/Shift-Tab to cycle, Enter to send",
        selected + 1,
        matches.len(),
        emoji.as_str(),
        emoji.name()
    )
}

fn slash_command_prefix(input: &str) -> Option<&str> {
    let input = input.trim_start();
    let without_slash = input.strip_prefix('/')?;
    (!without_slash.chars().any(char::is_whitespace)).then_some(without_slash)
}

fn react_command_emoji_prefix(input: &str) -> Option<&str> {
    let input = input.trim_start();
    let query = input.strip_prefix("/react ")?.trim();
    (!query.is_empty() && !query.chars().any(char::is_whitespace)).then_some(query)
}

/// Shared tail of every `/<command> <target>` prefix helper: `rest` is
/// whatever follows the command name (already stripped by the caller).
/// Bare `/<command>` (nothing typed yet) yields an empty target; otherwise
/// the command name must be followed by whitespace — `/logouty` is not
/// `/logout` — and the target is the whitespace-trimmed remainder.
fn target_after_command_name(rest: &str) -> Option<&str> {
    if rest.is_empty() {
        return Some("");
    }
    rest.chars()
        .next()
        .is_some_and(char::is_whitespace)
        .then(|| rest.trim_start())
}

/// `/<command> <target>` prefix extraction for the many commands that share
/// this exact shape (`/send`, `/logout`, `/delete`, `/recover`, `/backup enable`,
/// `/account`, `/verify`; `/room`'s `/switch` alias and `/filter`'s extra
/// no-embedded-whitespace constraint build on [`target_after_command_name`]
/// directly instead, since they aren't quite this shape).
fn command_target_prefix<'a>(input: &'a str, command: &str) -> Option<&'a str> {
    target_after_command_name(input.strip_prefix(command)?)
}

fn filter_target_prefix(input: &str) -> Option<&str> {
    command_target_prefix(input, "/filter")
        .filter(|target| !target.chars().any(char::is_whitespace))
}

fn filter_argument_candidates(target: &str) -> Vec<&'static str> {
    let target = target.to_lowercase();
    FILTER_ARGUMENTS
        .iter()
        .copied()
        .filter(|argument| argument.starts_with(&target))
        .collect()
}

fn slash_command_candidates(prefix: &str) -> Vec<SlashCommand> {
    let command_prefix = format!("/{prefix}");
    SLASH_COMMANDS
        .iter()
        .copied()
        .filter(|command| command.name.starts_with(&command_prefix))
        .collect()
}

fn send_target_prefix(input: &str) -> Option<&str> {
    command_target_prefix(input, "/send")
}

fn room_action_user_target_prefix(input: &str) -> Option<(&'static str, &str)> {
    ["/invite", "/kick", "/ban", "/unban"]
        .into_iter()
        .find_map(|command| command_target_prefix(input, command).map(|target| (command, target)))
}

/// `Some(target)` while the caller is still typing the path token; `None`
/// once caption text has started, since filename completion no longer
/// applies there. Delegates to [`command::send_argument_still_in_path_token`]
/// (the same tokenizer `parse_leading_path_token` uses) rather than a
/// separate whitespace check, so a backslash-escaped space mid-path (e.g.
/// `My\ File`) doesn't get mistaken for the start of caption text.
fn send_path_completion_token(target: &str) -> Option<&str> {
    command::send_argument_still_in_path_token(target).then_some(target)
}

/// Split a partial path into `(raw_prefix, fs_dir, basename)`: `raw_prefix`
/// is exactly what the user typed for the directory portion (so a literal
/// `~/` is preserved when re-inserted into the buffer), `fs_dir` is the same
/// directory with a leading `~/` expanded to `$HOME` for the actual
/// `read_dir` call, and `basename` is the partial filename to prefix-match.
fn send_path_dir_and_partial(partial: &str) -> (String, PathBuf, String) {
    let (raw_prefix, basename) = if partial.is_empty() || partial.ends_with('/') {
        (partial.to_owned(), String::new())
    } else {
        match partial.rfind('/') {
            Some(idx) => (partial[..=idx].to_owned(), partial[idx + 1..].to_owned()),
            None => (String::new(), partial.to_owned()),
        }
    };
    // raw_prefix is always either empty or ends in '/' (see above), so it can
    // never equal the bare string "~" — only the `~/`-prefixed form is
    // reachable here.
    let fs_dir = if raw_prefix.is_empty() {
        PathBuf::from(".")
    } else if let Some(rest) = raw_prefix.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|| PathBuf::from(&raw_prefix))
    } else {
        PathBuf::from(&raw_prefix)
    };
    (raw_prefix, fs_dir, basename)
}

/// List filesystem entries in `partial`'s directory whose name starts with
/// its basename, sorted, with a trailing `/` on directory candidates.
/// Synchronous disk IO is acceptable here: this runs on a Tab keypress (not
/// the render/key-handling hot path network calls must avoid), matching the
/// existing synchronous file IO already used by the config-editor flow.
fn send_path_candidates(partial: &str) -> Vec<String> {
    let (raw_prefix, fs_dir, basename) = send_path_dir_and_partial(partial);
    let Ok(entries) = std::fs::read_dir(&fs_dir) else {
        return Vec::new();
    };
    let mut candidates: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&basename) {
                return None;
            }
            let is_dir = entry.file_type().is_ok_and(|ft| ft.is_dir());
            Some(format!(
                "{raw_prefix}{name}{}",
                if is_dir { "/" } else { "" }
            ))
        })
        .collect();
    candidates.sort();
    candidates
}

/// Re-quote a filesystem path for insertion into the `/send` argument, using
/// backslash-escapes (not wrapping quotes) so it round-trips through
/// [`command::parse_leading_path_token`], which only unescapes `\ `, `\\`,
/// `\'`, and `\"`.
fn quote_path_for_send(path: &str) -> String {
    if !path.chars().any(|c| matches!(c, ' ' | '\'' | '"' | '\\')) {
        return path.to_owned();
    }
    let mut out = String::with_capacity(path.len());
    for ch in path.chars() {
        if matches!(ch, ' ' | '\'' | '"' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn room_target_prefix(input: &str) -> Option<&str> {
    let rest = input
        .strip_prefix("/room")
        .or_else(|| input.strip_prefix("/switch"))?;
    target_after_command_name(rest)
}

fn longest_common_prefix(candidates: &[String]) -> Option<String> {
    let first = candidates.first()?;
    let mut prefix_end = first.len();
    for candidate in &candidates[1..] {
        prefix_end = first[..prefix_end]
            .char_indices()
            .map(|(index, ch)| (index + ch.len_utf8(), &first[..index + ch.len_utf8()]))
            .take_while(|(_, prefix)| {
                candidate
                    .get(..prefix.len())
                    .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
            })
            .map(|(end, _)| end)
            .last()
            .unwrap_or(0);
        if prefix_end == 0 {
            break;
        }
    }
    Some(first[..prefix_end].to_owned())
}

fn room_completion_value(room: &RoomDto) -> String {
    room.canonical_alias
        .as_deref()
        .or(room.name.as_deref())
        .unwrap_or(&room.room_id)
        .to_owned()
}

fn room_matching_prefix_value(room: &RoomDto, target: &str) -> Option<String> {
    let target = target.trim();
    if target.is_empty() {
        return Some(room_completion_value(room));
    }
    let target_lower = target.to_lowercase();
    for field in [
        room.name.as_deref(),
        room.canonical_alias.as_deref(),
        Some(room.room_id.as_str()),
    ]
    .into_iter()
    .flatten()
    {
        if field.to_lowercase().starts_with(&target_lower) {
            return Some(field.to_owned());
        }
    }
    if let Some(alias) = room_alias_with_hash(target) {
        if let Some(field) = room
            .canonical_alias
            .as_deref()
            .filter(|field| field.to_lowercase().starts_with(&alias.to_lowercase()))
        {
            return Some(field.to_owned());
        }
    }
    let target_local = incomplete_matrix_room_name(target)?;
    room.canonical_alias
        .as_deref()
        .and_then(matrix_room_local_name)
        .filter(|local| {
            local
                .to_lowercase()
                .starts_with(&target_local.to_lowercase())
        })
        .or_else(|| {
            matrix_room_local_name(&room.room_id).filter(|local| {
                local
                    .to_lowercase()
                    .starts_with(&target_local.to_lowercase())
            })
        })
        .map(str::to_owned)
}

fn case_insensitive_suffix<'a>(candidate: &'a str, prefix: &str) -> Option<&'a str> {
    candidate
        .get(..prefix.len())
        .filter(|start| start.eq_ignore_ascii_case(prefix))?;
    candidate.get(prefix.len()..)
}

fn room_matches_completion(room: &RoomDto, target: &str) -> bool {
    room_matching_prefix_value(room, target).is_some()
}

fn incomplete_matrix_room_name(target: &str) -> Option<&str> {
    let target = target.trim();
    if target.is_empty() || target.contains(':') {
        return None;
    }
    Some(target.trim_start_matches(['#', '!'])).filter(|target| !target.is_empty())
}

fn room_alias_with_hash(target: &str) -> Option<String> {
    let target = target.trim();
    if target.is_empty() || target.starts_with(['#', '!']) || !target.contains(':') {
        return None;
    }
    Some(format!("#{target}"))
}

fn matrix_room_local_name(value: &str) -> Option<&str> {
    let value = value.strip_prefix(['#', '!'])?;
    value.split_once(':').map(|(local, _server)| local)
}

#[cfg(test)]
mod send_completion_tests {
    use super::{
        quote_path_for_send, send_path_candidates, send_path_completion_token,
        send_path_dir_and_partial, send_target_prefix,
    };
    use std::fs;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("axon-tui-send-completion-test-{name}"));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn send_target_prefix_requires_whitespace_after_command() {
        assert_eq!(send_target_prefix("/send"), Some(""));
        assert_eq!(send_target_prefix("/send "), Some(""));
        assert_eq!(send_target_prefix("/send photo.png"), Some("photo.png"));
        assert_eq!(send_target_prefix("/sendx"), None);
        assert_eq!(send_target_prefix("/room foo"), None);
    }

    #[test]
    fn completion_token_stops_once_caption_starts() {
        assert_eq!(send_path_completion_token("photo.png"), Some("photo.png"));
        assert_eq!(send_path_completion_token("photo.png caption"), None);
        // Still inside an unterminated quote: whitespace is part of the token.
        assert_eq!(send_path_completion_token("'my file"), Some("'my file"));
        // Quote closed: whatever follows is caption territory.
        assert_eq!(send_path_completion_token("'my file' caption"), None);
        // A backslash-escaped space is still part of the path, matching
        // parse_leading_path_token — not a signal that caption text started.
        assert_eq!(send_path_completion_token("My\\ File"), Some("My\\ File"));
        assert_eq!(send_path_completion_token("My\\ File caption"), None);
    }

    #[test]
    fn splits_directory_and_basename() {
        let (raw_prefix, fs_dir, basename) = send_path_dir_and_partial("photo");
        assert_eq!(raw_prefix, "");
        assert_eq!(fs_dir, std::path::PathBuf::from("."));
        assert_eq!(basename, "photo");

        let (raw_prefix, _fs_dir, basename) = send_path_dir_and_partial("/tmp/pho");
        assert_eq!(raw_prefix, "/tmp/");
        assert_eq!(basename, "pho");

        let (raw_prefix, _fs_dir, basename) = send_path_dir_and_partial("/tmp/");
        assert_eq!(raw_prefix, "/tmp/");
        assert_eq!(basename, "");
    }

    #[test]
    fn lists_matching_filesystem_entries_with_trailing_slash_on_dirs() {
        let dir = TempDir::new("list");
        fs::write(dir.0.join("photo.png"), b"").unwrap();
        fs::write(dir.0.join("photo.jpg"), b"").unwrap();
        fs::create_dir(dir.0.join("photos")).unwrap();
        fs::write(dir.0.join("other.txt"), b"").unwrap();

        let prefix = format!("{}/pho", dir.0.display());
        let candidates = send_path_candidates(&prefix);
        let base = dir.0.display().to_string();
        assert_eq!(
            candidates,
            vec![
                format!("{base}/photo.jpg"),
                format!("{base}/photo.png"),
                format!("{base}/photos/"),
            ]
        );
    }

    #[test]
    fn quoting_round_trips_through_the_shared_tokenizer() {
        for path in [
            "photo.png",
            "My Photo.png",
            "it's a photo.png",
            "quote\".png",
            "back\\slash.png",
        ] {
            let quoted = quote_path_for_send(path);
            let (parsed, caption) = crate::command::parse_leading_path_token(&quoted);
            assert_eq!(parsed, path, "round-trip failed for {path:?}");
            assert_eq!(caption, None);
        }
    }
}
