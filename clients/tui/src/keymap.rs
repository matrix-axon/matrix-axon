use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::api::AccountDto;
use crate::app::emoji_matches;
use crate::app::{
    cycle_index, AccountSelection, App, Mode, PopupKind, RoomFilter, RoomSort, SearchKind, Status,
    VerificationStage,
};
use crate::command;
use crate::command::HELP_COMMANDS;
use crate::search::{SearchFormField, SEARCH_HELP_TEXT};

impl App {
    pub(crate) async fn handle_key(&mut self, key: KeyEvent) -> bool {
        if self.shortcuts.quit.matches(key) {
            self.should_quit = true;
        } else if self.shortcuts.focus_next.matches(key) {
            self.cycle_focus();
        } else if self.shortcuts.focus_prev.matches(key) {
            self.cycle_focus_prev();
        } else if self.shortcuts.next_room.matches(key) {
            self.dismiss_input_help();
            self.abandon_transient_input_mode();
            self.switch_relative_room(1).await;
        } else if self.shortcuts.previous_room.matches(key) {
            self.dismiss_input_help();
            self.abandon_transient_input_mode();
            self.switch_relative_room(-1).await;
        } else if self.shortcuts.next_account.matches(key) && self.accounts_panel_visible() {
            self.dismiss_input_help();
            self.abandon_transient_input_mode();
            self.cycle_account(1);
            self.load_selected_timeline().await;
        } else if self.shortcuts.previous_account.matches(key) && self.accounts_panel_visible() {
            self.dismiss_input_help();
            self.abandon_transient_input_mode();
            self.cycle_account(-1);
            self.load_selected_timeline().await;
        } else if self.shortcuts.message_down.matches(key) && self.mode != Mode::DateJump {
            self.dismiss_input_help();
            self.abandon_transient_input_mode();
            self.move_selected_message(1);
            self.mode = Mode::MessageList;
        } else if self.shortcuts.message_up.matches(key) && self.mode != Mode::DateJump {
            self.dismiss_input_help();
            self.abandon_transient_input_mode();
            let at_first = self.is_at_first_message();
            self.move_selected_message(-1);
            self.mode = Mode::MessageList;
            if at_first {
                self.load_more_history().await;
            }
        } else if self.shortcuts.toggle_accounts_panel.matches(key) && !self.is_mid_command() {
            self.toggle_accounts_panel();
        } else if self.shortcuts.toggle_rooms_panel.matches(key) && !self.is_mid_command() {
            self.toggle_rooms_panel();
        } else if self.shortcuts.toggle_unread_filter.matches(key) && !self.is_mid_command() {
            self.toggle_unread_filter();
        } else if self.shortcuts.room_filter_cycle.matches(key) && !self.is_mid_command() {
            self.cycle_room_filter();
        } else if self.shortcuts.room_sort_cycle.matches(key) && !self.is_mid_command() {
            self.cycle_room_sort();
        } else if self.shortcuts.room_filter_dms.matches(key) && !self.is_mid_command() {
            self.set_room_filter(RoomFilter::Dms);
        } else if self.shortcuts.room_filter_groups.matches(key) && !self.is_mid_command() {
            self.set_room_filter(RoomFilter::Groups);
        } else if self.shortcuts.room_filter_favorites.matches(key) && !self.is_mid_command() {
            self.set_room_filter(RoomFilter::Favorites);
        } else if self.shortcuts.room_filter_all.matches(key) && !self.is_mid_command() {
            self.set_room_filter(RoomFilter::All);
        } else if self.shortcuts.room_filter_by_name.matches(key) && !self.is_mid_command() {
            self.begin_room_name_filter();
        } else if self.shortcuts.room_sort_recent.matches(key) && !self.is_mid_command() {
            // Repeat toggles between the two activity directions.
            self.set_room_sort(if self.room_sort == RoomSort::RecentActivity {
                RoomSort::OldestActivity
            } else {
                RoomSort::RecentActivity
            });
        } else if self.shortcuts.room_sort_alpha.matches(key) && !self.is_mid_command() {
            // Repeat toggles between A–Z and Z–A.
            self.set_room_sort(if self.room_sort == RoomSort::AlphaAsc {
                RoomSort::AlphaDesc
            } else {
                RoomSort::AlphaAsc
            });
        } else if self.shortcuts.unread_threads.matches(key) && !self.is_mid_command() {
            self.open_unread_threads_picker();
        } else if self.shortcuts.refresh.matches(key) && !self.is_mid_command() {
            self.request_rooms_refresh();
        } else {
            match self.mode.clone() {
                Mode::Compose => self.handle_compose_key(key).await,
                Mode::LoginUsername => self.handle_login_username_key(key).await,
                Mode::LoginPassword {
                    username,
                    homeserver,
                } => {
                    self.handle_login_password_key(key, username, homeserver)
                        .await
                }
                Mode::RecoveryKey { account, origin } => {
                    self.handle_recovery_key(key, account, origin)
                }
                Mode::ConfirmLogout { account } => self.handle_confirm_logout_key(key, account),
                Mode::ConfirmDelete { account } => self.handle_confirm_delete_key(key, account),
                Mode::ConfirmRoomAction { action } => {
                    self.handle_confirm_room_action_key(key, action)
                }
                Mode::RoomList => self.handle_room_list_key(key).await,
                Mode::AccountList => self.handle_account_list_key(key).await,
                Mode::MessageList => self.handle_message_list_key(key).await,
                Mode::Search(kind, query) => self.handle_search_key(key, kind, query).await,
                Mode::SearchForm => self.handle_search_form_key(key).await,
                Mode::SearchResults => self.handle_search_results_key(key).await,
                Mode::Editing { event_id } => self.handle_editing_key(key, event_id).await,
                Mode::Reacting { event_id } => self.handle_reacting_key(key, event_id).await,
                Mode::Unreacting {
                    target_event_id,
                    choices,
                    selected,
                } => {
                    self.handle_unreacting_key(key, target_event_id, choices, selected)
                        .await
                }
                Mode::Verification => self.handle_verification_key(key),
                Mode::Popup(kind) => self.handle_popup_key(key, kind).await,
                Mode::DateJump => self.handle_date_jump_key(key).await,
            }
        }
        self.should_quit
    }

    /// Key handling for the SAS verification modal (ADR 0028 §2). Literal
    /// `y`/`n`/`Esc`, like the logout confirmation — no configurable shortcut.
    /// `y` only confirms once the SAS emoji are on screen; `n`/`Esc` cancel a
    /// live flow; once terminal, any of `Esc`/`Enter`/`q` dismiss the modal.
    fn handle_verification_key(&mut self, key: KeyEvent) {
        let Some(flow) = self.verification.as_ref() else {
            self.mode = Mode::Compose;
            return;
        };
        if flow.stage.is_terminal() {
            if self.shortcuts.clear_input.matches(key)
                || matches!(
                    key.code,
                    KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('Q')
                )
            {
                self.close_verification();
            }
            return;
        }
        let can_confirm = flow.stage == VerificationStage::Compare;
        if can_confirm && matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            self.confirm_active_verification();
        } else if matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N'))
            || self.shortcuts.clear_input.matches(key)
        {
            self.cancel_active_verification();
        } else if matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) {
            // Manual resync: re-read the authoritative flow state from the server.
            // Useful when the WS sas frame may have been missed or dropped.
            self.resync_active_verification();
            self.status = Status::from("verification: resyncing with server…".to_owned());
        }
    }

    /// Close the verification modal and return to compose.
    fn close_verification(&mut self) {
        self.verification = None;
        self.mode = Mode::Compose;
        self.status = Status::Info(String::new());
    }

    async fn handle_popup_key(&mut self, key: KeyEvent, kind: PopupKind) {
        if self.shortcuts.clear_input.matches(key) {
            let was_search_help = kind == PopupKind::CommandResponse
                && self.pending_command_response.as_deref() == Some(SEARCH_HELP_TEXT);
            if kind == PopupKind::MediaPreview {
                self.mode = Mode::MessageList;
                // Signal draw() to clear the former popup area.  Sixel/iTerm2
                // pixels survive ratatui's cell-diff pass and leave a ghost
                // image unless we explicitly clear the region on close.
                self.clear_media_preview = true;
            } else {
                self.mode = Mode::Compose;
            }
            self.popup_scroll = 0;
            self.help_selection = 0;
            if kind == PopupKind::CommandResponse {
                self.pending_command_response = None;
                if was_search_help {
                    self.status = Status::Info(String::new());
                    self.clear_input_buffer();
                }
            }
        } else if kind == PopupKind::Help {
            self.handle_help_popup_key(key);
        } else if kind == PopupKind::UnreadThreads {
            self.handle_unread_threads_popup_key(key).await;
        } else if key.code == KeyCode::Up {
            self.popup_scroll = self.popup_scroll.saturating_sub(1);
        } else if key.code == KeyCode::Down {
            self.popup_scroll = self.popup_scroll.saturating_add(1);
        } else if key.code == KeyCode::PageUp || self.shortcuts.message_page_up.matches(key) {
            self.popup_scroll = self.popup_scroll.saturating_sub(8);
        } else if key.code == KeyCode::PageDown || self.shortcuts.message_page_down.matches(key) {
            self.popup_scroll = self.popup_scroll.saturating_add(8);
        }
    }

    async fn handle_unread_threads_popup_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Up {
            self.move_unread_thread_selection(-1);
        } else if key.code == KeyCode::Down {
            self.move_unread_thread_selection(1);
        } else if key.code == KeyCode::PageUp || self.shortcuts.message_page_up.matches(key) {
            self.move_unread_thread_selection(-8);
        } else if key.code == KeyCode::PageDown || self.shortcuts.message_page_down.matches(key) {
            self.move_unread_thread_selection(8);
        } else if self.shortcuts.submit.matches(key) {
            self.open_selected_unread_thread().await;
        }
    }

    fn handle_help_popup_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Up {
            self.help_selection = self
                .help_selection
                .checked_sub(1)
                .unwrap_or_else(|| HELP_COMMANDS.len().saturating_sub(1));
        } else if key.code == KeyCode::Down {
            self.help_selection = (self.help_selection + 1) % HELP_COMMANDS.len().max(1);
        } else if self.shortcuts.submit.matches(key) {
            let command = HELP_COMMANDS
                .get(self.help_selection)
                .unwrap_or(&HELP_COMMANDS[0]);
            self.input.buffer = command.insert_text.to_owned();
            self.input.cursor = self.input.buffer.len();
            self.input.react_tab = None;
            self.show_input_help = false;
            self.mode = Mode::Compose;
            self.popup_scroll = 0;
            self.status = format!("selected command: {}", command.label).into();
        }
    }

    async fn handle_search_results_key(&mut self, key: KeyEvent) {
        if self.shortcuts.clear_input.matches(key) {
            self.clear_search_results();
        } else if self.shortcuts.submit.matches(key) {
            self.jump_to_selected_search_result().await;
        } else if key.code == KeyCode::Up {
            self.move_search_result_selection(-1).await;
        } else if key.code == KeyCode::Down {
            self.move_search_result_selection(1).await;
        } else if key.code == KeyCode::PageUp || self.shortcuts.message_page_up.matches(key) {
            self.page_search_result_selection(-1).await;
        } else if key.code == KeyCode::PageDown || self.shortcuts.message_page_down.matches(key) {
            self.page_search_result_selection(1).await;
        } else if key.code == KeyCode::Home {
            self.jump_to_search_result_edge(false).await;
        } else if key.code == KeyCode::End {
            self.jump_to_search_result_edge(true).await;
        } else if key.code == KeyCode::Char('n') && key.modifiers.is_empty() {
            self.move_search_result_selection(1).await;
        } else if key.code == KeyCode::Char('N') && key.modifiers == KeyModifiers::SHIFT {
            self.move_search_result_selection(-1).await;
        } else if self.shortcuts.search_sort.matches(key) {
            self.toggle_search_result_sort_order().await;
        } else if self.shortcuts.search_group.matches(key) {
            self.toggle_search_result_grouping().await;
        } else if self.shortcuts.search_edit.matches(key) {
            self.edit_current_search();
        } else if self.shortcuts.reply.matches(key) {
            self.reply_to_selected_search_result().await;
        } else if self.shortcuts.thread.matches(key) {
            self.thread_from_selected_search_result().await;
        }
    }

    async fn handle_search_form_key(&mut self, key: KeyEvent) {
        if self.shortcuts.clear_input.matches(key) {
            self.pending_search = None;
            self.mode = if self.search_results.is_some() {
                Mode::SearchResults
            } else {
                Mode::Compose
            };
            self.search_form.error = None;
            self.status = Status::from(String::new());
        } else if self.shortcuts.submit.matches(key) {
            self.submit_search_form().await;
        } else if self.shortcuts.complete.matches(key)
            || (key.code == KeyCode::Down && key.modifiers.is_empty())
        {
            self.search_form.next_field(false);
        } else if key.code == KeyCode::BackTab
            || (key.code == KeyCode::Up && key.modifiers.is_empty())
        {
            self.search_form.next_field(true);
        } else if self.search_form.field == SearchFormField::Scope
            && key.code == KeyCode::Left
            && key.modifiers.is_empty()
        {
            self.search_form.cycle_scope(true);
        } else if self.search_form.field == SearchFormField::Scope
            && (key.code == KeyCode::Right || key.code == KeyCode::Char(' '))
            && key.modifiers.is_empty()
        {
            self.search_form.cycle_scope(false);
        } else if key.code == KeyCode::Left && key.modifiers == KeyModifiers::ALT {
            self.search_form.cycle_scope(true);
        } else if key.code == KeyCode::Right && key.modifiers == KeyModifiers::ALT {
            self.search_form.cycle_scope(false);
        } else if self.shortcuts.backspace.matches(key) || key.code == KeyCode::Delete {
            self.search_form_backspace();
        } else if key.code == KeyCode::Char('u') && key.modifiers == KeyModifiers::CONTROL {
            if let Some(buffer) = self.search_form.edit_buffer_mut() {
                buffer.clear();
            }
        } else if let KeyCode::Char(ch) = key.code {
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                self.search_form_insert(ch);
            }
        }
    }

    async fn handle_search_key(&mut self, key: KeyEvent, kind: SearchKind, mut query: String) {
        if self.shortcuts.clear_input.matches(key) {
            self.reset_search_list_scroll(&kind);
            self.clear_search_status();
            if kind == SearchKind::RoomNameFilter {
                // Esc abandons the in-progress name filter and restores whatever
                // filter was active before input started (ADR 0042).
                self.cancel_room_name_filter();
            }
            self.mode = self.list_mode_for(kind);
        } else if self.shortcuts.submit.matches(key) {
            self.reset_search_list_scroll(&kind);
            self.mode = self.list_mode_for(kind);
            match kind {
                SearchKind::Rooms => self.commit_room_search(query).await,
                SearchKind::Messages => self.commit_message_search(query),
                SearchKind::Accounts => {
                    if self.commit_account_search(query) {
                        let search_status =
                            std::mem::replace(&mut self.status, Status::Info(String::new()));
                        self.load_selected_timeline().await;
                        self.status = search_status;
                    }
                }
                // The filter is already live; Enter just confirms it and clears
                // the saved fallback so a later Esc elsewhere can't revert it.
                SearchKind::RoomNameFilter => self.room_filter_before_input = None,
            }
        } else if self.shortcuts.backspace.matches(key) || key.code == KeyCode::Delete {
            query.pop();
            self.reset_search_list_scroll(&kind);
            if kind == SearchKind::RoomNameFilter {
                self.update_room_name_filter(query.clone());
            }
            self.mode = Mode::Search(kind, query);
        } else if let KeyCode::Char(ch) = key.code {
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                query.push(ch);
                self.reset_search_list_scroll(&kind);
                if kind == SearchKind::RoomNameFilter {
                    self.update_room_name_filter(query.clone());
                }
                self.mode = Mode::Search(kind, query);
            }
        }
    }

    /// The list mode a search/filter input returns to on Esc/Enter.
    fn list_mode_for(&self, kind: SearchKind) -> Mode {
        match kind {
            SearchKind::Rooms | SearchKind::RoomNameFilter => Mode::RoomList,
            SearchKind::Messages => Mode::MessageList,
            SearchKind::Accounts => Mode::AccountList,
        }
    }

    fn reset_search_list_scroll(&mut self, kind: &SearchKind) {
        match kind {
            SearchKind::Rooms | SearchKind::RoomNameFilter => self.rooms.scroll = 0,
            SearchKind::Accounts => self.accounts.scroll = 0,
            SearchKind::Messages => {}
        }
    }

    fn clear_search_status(&mut self) {
        if self.last_search.is_some() {
            self.status = Status::Info(String::new());
        }
    }

    async fn handle_room_list_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Left && key.modifiers == KeyModifiers::ALT {
            self.adjust_rooms_width(-2);
        } else if key.code == KeyCode::Right && key.modifiers == KeyModifiers::ALT {
            self.adjust_rooms_width(2);
        } else if key.code == KeyCode::Up {
            self.switch_relative_room(-1).await;
        } else if key.code == KeyCode::Down {
            self.switch_relative_room(1).await;
        } else if key.code == KeyCode::PageUp || self.shortcuts.message_page_up.matches(key) {
            let page = self.rooms.page_size.max(1) as isize;
            self.switch_relative_room(-page).await;
        } else if key.code == KeyCode::PageDown || self.shortcuts.message_page_down.matches(key) {
            let page = self.rooms.page_size.max(1) as isize;
            self.switch_relative_room(page).await;
        } else if key.code == KeyCode::Home {
            let visible = self.visible_room_indices();
            if let Some(&first) = visible.first() {
                self.rooms.selected = Some(first);
                self.load_selected_timeline().await;
            }
        } else if key.code == KeyCode::End {
            let visible = self.visible_room_indices();
            if let Some(&last) = visible.last() {
                self.rooms.selected = Some(last);
                self.load_selected_timeline().await;
            }
        } else if self.shortcuts.pin_room.matches(key) {
            self.pin_room(None);
        } else if self.shortcuts.unpin_room.matches(key) {
            self.unpin_room(None);
        } else if self.shortcuts.find.matches(key) {
            self.rooms.scroll = 0;
            self.mode = Mode::Search(SearchKind::Rooms, String::new());
        } else if key.code == KeyCode::Char('/') && key.modifiers.is_empty() {
            self.clear_input_buffer();
            self.insert_char('/');
            self.mode = Mode::Compose;
        } else if key.code == KeyCode::Char('n') && key.modifiers.is_empty() {
            if let Some(q) = self.last_search.clone() {
                self.search_adjacent_room(&q, true).await;
            }
        } else if key.code == KeyCode::Char('N') && key.modifiers == KeyModifiers::SHIFT {
            if let Some(q) = self.last_search.clone() {
                self.search_adjacent_room(&q, false).await;
            }
        } else if self.shortcuts.submit.matches(key) || self.shortcuts.clear_input.matches(key) {
            self.clear_search_status();
            self.mode = Mode::Compose;
        }
    }

    async fn handle_message_list_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Up || self.shortcuts.message_up.matches(key) {
            self.dismiss_input_help();
            let at_first = self.is_at_first_message();
            self.move_selected_message(-1);
            if at_first {
                self.load_more_history().await;
            }
        } else if key.code == KeyCode::Down || self.shortcuts.message_down.matches(key) {
            self.dismiss_input_help();
            let at_last = self.is_at_last_message();
            self.move_selected_message(1);
            if at_last {
                self.load_newer_history().await;
            }
        } else if self.shortcuts.jump_day_back.matches(key) {
            self.jump_adjacent_day(false).await;
        } else if self.shortcuts.jump_day_forward.matches(key) {
            self.jump_adjacent_day(true).await;
        } else if key.code == KeyCode::PageUp || self.shortcuts.message_page_up.matches(key) {
            self.dismiss_input_help();
            let at_top = self.messages.scroll == 0;
            self.page_selected_message(-1);
            if at_top {
                self.load_more_history().await;
            }
        } else if key.code == KeyCode::PageDown || self.shortcuts.message_page_down.matches(key) {
            self.dismiss_input_help();
            let at_last = self.is_at_last_message();
            self.page_selected_message(1);
            if at_last {
                self.load_newer_history().await;
            }
        } else if key.code == KeyCode::Home {
            self.dismiss_input_help();
            self.jump_to_first_message();
        } else if key.code == KeyCode::End {
            self.dismiss_input_help();
            // From a historical jump, End returns to the live tail (reloads the
            // newest page); otherwise it just drops to the bottom of the buffer.
            if self.last_jump_ts.is_some() {
                self.load_selected_timeline().await;
            }
            self.jump_to_last_message();
        } else if key.code == KeyCode::Char('J') && key.modifiers == KeyModifiers::SHIFT {
            self.start_date_jump();
        } else if self.shortcuts.find.matches(key) {
            self.mode = Mode::Search(SearchKind::Messages, String::new());
        } else if key.code == KeyCode::Char('/') && key.modifiers.is_empty() {
            self.clear_input_buffer();
            self.insert_char('/');
            self.mode = Mode::Compose;
        } else if key.code == KeyCode::Char('n') && key.modifiers.is_empty() {
            if let Some(q) = self.last_search.clone() {
                self.search_adjacent_message(&q, true);
            }
        } else if key.code == KeyCode::Char('N') && key.modifiers == KeyModifiers::SHIFT {
            if let Some(q) = self.last_search.clone() {
                self.search_adjacent_message(&q, false);
            }
        } else if self.shortcuts.reply.matches(key) {
            self.dismiss_input_help();
            self.start_reply_to_selected_message();
        } else if self.shortcuts.thread.matches(key) {
            self.dismiss_input_help();
            self.start_thread_from_selected_message().await;
        } else if self.shortcuts.edit_message.matches(key) {
            self.dismiss_input_help();
            self.start_edit_selected_message();
        } else if self.shortcuts.redact_message.matches(key) {
            self.dismiss_input_help();
            self.redact_selected_message().await;
        } else if self.shortcuts.react_message.matches(key) {
            self.dismiss_input_help();
            self.start_react_to_selected_message();
        } else if self.shortcuts.unreact_message.matches(key) {
            self.dismiss_input_help();
            self.start_unreact_from_selected_message().await;
        } else if self.shortcuts.media_preview.matches(key) {
            self.dismiss_input_help();
            self.open_selected_media_preview();
        } else if self.shortcuts.clear_input.matches(key) {
            // Esc leaves the thread panel first (ADR 0032 M2); a second Esc
            // returns focus to compose as usual.
            if !self.close_thread_panel() {
                self.clear_search_status();
                self.mode = Mode::Compose;
            }
        }
    }

    async fn handle_compose_key(&mut self, key: KeyEvent) {
        if self.handle_input_navigation_key(key) {
            return;
        }
        if key.code == KeyCode::Up && key.modifiers == KeyModifiers::ALT {
            self.adjust_input_lines(1);
        } else if key.code == KeyCode::Down && key.modifiers == KeyModifiers::ALT {
            self.adjust_input_lines(-1);
        } else if self.shortcuts.edit_previous.matches(key) {
            self.dismiss_input_help();
            self.move_selected_message(-1);
            self.mode = Mode::MessageList;
        } else if self.shortcuts.edit_next.matches(key) {
            self.dismiss_input_help();
            self.move_selected_message(1);
            self.mode = Mode::MessageList;
        } else if self.shortcuts.jump_day_back.matches(key) {
            self.jump_adjacent_day(false).await;
            self.mode = Mode::MessageList;
        } else if self.shortcuts.jump_day_forward.matches(key) {
            self.jump_adjacent_day(true).await;
            self.mode = Mode::MessageList;
        } else if key.code == KeyCode::Char('J')
            && key.modifiers == KeyModifiers::SHIFT
            && self.input.buffer.is_empty()
        {
            // Shift+J opens the date-jump prompt — but only on an empty input, so
            // mid-message typing of a capital J is untouched. This makes the
            // shortcut reachable from Compose (like the day-jump keys) without the
            // old footgun where it was typed as text and then sent on Enter.
            self.start_date_jump();
        } else if self.shortcuts.message_page_up.matches(key) {
            self.dismiss_input_help();
            self.page_selected_message(-1);
            if self.messages.selection.is_some() {
                self.mode = Mode::MessageList;
            }
        } else if self.shortcuts.message_page_down.matches(key) {
            self.dismiss_input_help();
            self.page_selected_message(1);
            if self.messages.selection.is_some() {
                self.mode = Mode::MessageList;
            }
        } else if self.shortcuts.newline.matches(key) {
            // The configurable newline key (default Alt+Enter) inserts a hard line
            // break for multi-line messages; plain Enter still submits. Alt+Enter
            // is the default because it is reported via the legacy Alt(ESC) prefix
            // on terminals (and through tmux) that do not deliver a distinct
            // Shift+Enter; users on terminals that do can rebind it to Shift+Enter.
            self.dismiss_input_help();
            self.insert_char('\n');
        } else if self.shortcuts.submit.matches(key) {
            self.dismiss_input_help();
            if let Some(completions) = self.input.partial_room_completions.as_ref() {
                self.status = format!(
                    "room completion is partial: {} - type more or press Tab",
                    completions.join(", ")
                )
                .into();
                return;
            }
            let input = self.take_input_for_submit();
            self.handle_command(command::parse(&input)).await;
        } else if self.shortcuts.clear_input.matches(key) {
            self.clear_input_and_selection();
        } else if self.shortcuts.complete.matches(key) {
            self.dismiss_input_help();
            self.complete_input();
        } else if key.code == KeyCode::BackTab {
            // BackTab is the fixed reverse gesture for the configurable completion key.
            self.dismiss_input_help();
            self.complete_input_reverse();
        } else if key.code == KeyCode::Char('u') && key.modifiers == KeyModifiers::CONTROL {
            self.clear_input_buffer();
        } else if let KeyCode::Char(ch) = key.code {
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                self.dismiss_input_help();
                self.insert_char(ch);
            }
        }
    }

    async fn handle_login_username_key(&mut self, key: KeyEvent) {
        if self.handle_input_navigation_key(key) {
            return;
        }
        if self.shortcuts.submit.matches(key) {
            self.submit_login_username().await;
        } else if self.shortcuts.clear_input.matches(key) {
            self.cancel_lifecycle_input();
        } else if key.code == KeyCode::Char('u') && key.modifiers == KeyModifiers::CONTROL {
            self.clear_input_buffer();
        } else if let KeyCode::Char(ch) = key.code {
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                self.insert_char(ch);
            }
        }
    }

    async fn handle_login_password_key(
        &mut self,
        key: KeyEvent,
        username: String,
        homeserver: Option<String>,
    ) {
        if self.handle_input_navigation_key(key) {
            return;
        }
        if self.shortcuts.submit.matches(key) {
            self.submit_login_password(username, homeserver).await;
        } else if self.shortcuts.clear_input.matches(key) {
            self.cancel_lifecycle_input();
        } else if key.code == KeyCode::Char('u') && key.modifiers == KeyModifiers::CONTROL {
            self.clear_input_buffer();
        } else if let KeyCode::Char(ch) = key.code {
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                self.insert_char(ch);
            }
        }
    }

    fn handle_recovery_key(
        &mut self,
        key: KeyEvent,
        account: AccountDto,
        origin: crate::app::RecoveryOrigin,
    ) {
        if self.handle_input_navigation_key(key) {
            return;
        }
        if self.shortcuts.submit.matches(key) {
            self.submit_recovery_key(account, origin);
        } else if self.shortcuts.clear_input.matches(key) {
            self.cancel_recovery_input(account, origin);
        } else if key.code == KeyCode::Char('u') && key.modifiers == KeyModifiers::CONTROL {
            self.clear_input_buffer();
        } else if let KeyCode::Char(ch) = key.code {
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                self.insert_char(ch);
            }
        }
    }

    fn handle_confirm_logout_key(&mut self, key: KeyEvent, account: AccountDto) {
        // Safe default: only an explicit "y" confirms; "n", Esc, or the
        // clear-input shortcut cancel; every other key is ignored so a stray
        // press can't log anyone out.
        if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            self.perform_logout(account);
        } else if matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N'))
            || self.shortcuts.clear_input.matches(key)
        {
            self.cancel_logout_confirmation();
        }
    }

    fn handle_confirm_delete_key(&mut self, key: KeyEvent, account: AccountDto) {
        if self.handle_input_navigation_key(key) {
            return;
        }
        // "YES" confirms; "yes" (wrong case) clears the buffer and stays in
        // this mode with a hint so the user can retry; anything else cancels.
        if self.shortcuts.submit.matches(key) {
            let input = self.input.buffer.trim().to_owned();
            if input == "YES" {
                self.perform_delete(account);
            } else if input.eq_ignore_ascii_case("yes") {
                self.clear_input_buffer();
                self.status = Status::Info("type YES in all caps to confirm".to_owned());
                self.mode = Mode::ConfirmDelete { account };
            } else {
                self.cancel_delete_confirmation();
            }
        } else if self.shortcuts.clear_input.matches(key) {
            self.cancel_delete_confirmation();
        } else if key.code == KeyCode::Backspace {
            self.backspace();
        } else if let KeyCode::Char(ch) = key.code {
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                self.insert_char(ch);
            }
        }
    }

    fn handle_confirm_room_action_key(
        &mut self,
        key: KeyEvent,
        action: crate::app::PendingRoomAction,
    ) {
        if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            self.perform_room_action(action);
        } else if matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N'))
            || self.shortcuts.clear_input.matches(key)
        {
            self.cancel_room_action_confirmation();
        }
    }

    async fn handle_editing_key(&mut self, key: KeyEvent, event_id: String) {
        if self.handle_input_navigation_key(key) {
            return;
        }
        if self.shortcuts.submit.matches(key) {
            self.dismiss_input_help();
            let input = self.take_input_for_submit();
            self.mode = Mode::Compose;
            self.send_edit(&event_id, &input).await;
            // Back in compose: restore the room's draft into the now-empty
            // buffer the edit borrowed (M12).
            self.restore_draft_into_buffer();
        } else if self.shortcuts.clear_input.matches(key) {
            self.clear_input_and_selection();
        } else if key.code == KeyCode::Char('u') && key.modifiers == KeyModifiers::CONTROL {
            self.clear_input_buffer();
        } else if let KeyCode::Char(ch) = key.code {
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                self.dismiss_input_help();
                self.insert_char(ch);
            }
        }
    }

    async fn handle_reacting_key(&mut self, key: KeyEvent, event_id: String) {
        if self.shortcuts.backspace.matches(key) {
            self.dismiss_input_help();
            self.backspace();
            self.input.react_tab = None;
            self.update_react_status(&event_id);
            return;
        }
        if key.code == KeyCode::Delete {
            self.dismiss_input_help();
            self.delete_forward();
            self.input.react_tab = None;
            self.update_react_status(&event_id);
            return;
        }
        if self.handle_input_navigation_key(key) {
            return;
        }
        if self.shortcuts.submit.matches(key) {
            self.dismiss_input_help();
            let input = self.input.buffer.clone();
            let Some(reaction_key) = self.take_reaction_key(&input) else {
                self.input.react_tab = None;
                self.update_react_status(&event_id);
                return;
            };
            self.take_input_for_submit();
            self.mode = Mode::Compose;
            self.send_react(&event_id, &reaction_key).await;
            // Back in compose: restore the room's draft into the now-empty
            // buffer the reaction borrowed (M12).
            self.restore_draft_into_buffer();
        } else if self.shortcuts.clear_input.matches(key) {
            self.clear_input_and_selection();
        } else if self.shortcuts.complete.matches(key) {
            self.dismiss_input_help();
            self.complete_react_input(false);
        } else if key.code == KeyCode::BackTab {
            // BackTab is the fixed reverse gesture for the configurable completion key.
            self.dismiss_input_help();
            self.complete_react_input(true);
        } else if key.code == KeyCode::Char('u') && key.modifiers == KeyModifiers::CONTROL {
            self.clear_input_buffer();
            self.input.react_tab = None;
            self.update_react_status(&event_id);
        } else if let KeyCode::Char(ch) = key.code {
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                self.dismiss_input_help();
                self.insert_char(ch);
                self.input.react_tab = None;
                self.update_react_status(&event_id);
            }
        }
    }

    async fn handle_unreacting_key(
        &mut self,
        key: KeyEvent,
        target_event_id: String,
        choices: Vec<crate::app::OwnReaction>,
        selected: usize,
    ) {
        if self.shortcuts.clear_input.matches(key) {
            self.mode = Mode::Compose;
            self.status = "unreact canceled".into();
        } else if self.shortcuts.complete.matches(key) || key.code == KeyCode::BackTab {
            // BackTab is the fixed reverse gesture for the configurable completion key.
            let next = cycle_index(selected, choices.len(), key.code == KeyCode::BackTab);
            self.status = crate::app::unreact_selection_status(&choices, next);
            self.mode = Mode::Unreacting {
                target_event_id,
                choices,
                selected: next,
            };
        } else if self.shortcuts.submit.matches(key) {
            let reaction = choices[selected].clone();
            self.mode = Mode::Compose;
            self.withdraw_reaction(&target_event_id, reaction).await;
        }
    }

    fn handle_input_navigation_key(&mut self, key: KeyEvent) -> bool {
        if self.shortcuts.cursor_start.matches(key) || matches!(key.code, KeyCode::Home) {
            self.dismiss_input_help();
            self.move_cursor_to_start();
        } else if self.shortcuts.cursor_end.matches(key) || matches!(key.code, KeyCode::End) {
            self.dismiss_input_help();
            self.move_cursor_to_end();
        } else if key.code == KeyCode::Left && key.modifiers == KeyModifiers::CONTROL {
            self.dismiss_input_help();
            self.move_cursor_word_left();
        } else if key.code == KeyCode::Right && key.modifiers == KeyModifiers::CONTROL {
            self.dismiss_input_help();
            self.move_cursor_word_right();
        } else if self.shortcuts.cursor_left.matches(key) {
            self.dismiss_input_help();
            self.move_cursor_left();
        } else if self.shortcuts.cursor_right.matches(key) {
            self.dismiss_input_help();
            self.move_cursor_right();
        } else if key.modifiers == KeyModifiers::CONTROL
            && matches!(key.code, KeyCode::Char('w') | KeyCode::Backspace)
        {
            self.dismiss_input_help();
            self.delete_word_back();
        } else if self.shortcuts.backspace.matches(key) {
            self.dismiss_input_help();
            self.backspace();
        } else if key.code == KeyCode::Delete {
            self.dismiss_input_help();
            self.delete_forward();
        } else {
            return false;
        }
        true
    }

    pub(crate) fn take_input_for_submit(&mut self) -> String {
        let input = std::mem::take(&mut self.input.buffer);
        self.input.cursor = 0;
        self.reset_completion_state();
        input
    }

    pub(crate) fn take_reaction_key(&mut self, input: &str) -> Option<String> {
        let input = input.trim();
        if let Some(emoji) = emojis::get(input).or_else(|| emojis::get_by_shortcode(input)) {
            self.input.react_tab = None;
            return Some(emoji.as_str().to_owned());
        }
        if let Some(idx) = self.input.react_tab.take() {
            return emoji_matches(input).get(idx).map(|e| e.as_str().to_owned());
        }
        let matches = emoji_matches(input);
        match matches.as_slice() {
            [single] => Some(single.as_str().to_owned()),
            _ => None,
        }
    }

    pub(crate) fn clear_input_buffer(&mut self) {
        self.dismiss_input_help();
        self.input.buffer.clear();
        self.input.cursor = 0;
        self.reset_completion_state();
    }

    fn clear_input_and_selection(&mut self) {
        self.clear_input_buffer();
        self.input.react_tab = None;
        // Cancel any pending reply/thread compose target (ADR 0032 M4).
        self.pending_reply = None;
        self.pending_thread = None;
        self.mode = Mode::Compose;
        // Esc from the input box while a thread is open dismisses the thread and
        // returns to the main message pane (ADR 0032 M2). close_thread_panel
        // restores the prior selection/scroll and sets its own status.
        if self.close_thread_panel() {
            return;
        }
        self.messages.selection = None;
        self.messages.scroll = usize::MAX;
        self.status = Status::Info(String::new());
    }

    fn abandon_transient_input_mode(&mut self) {
        if matches!(
            self.mode,
            Mode::LoginUsername
                | Mode::LoginPassword { .. }
                | Mode::RecoveryKey { .. }
                | Mode::ConfirmLogout { .. }
                | Mode::ConfirmDelete { .. }
                | Mode::ConfirmRoomAction { .. }
                | Mode::Editing { .. }
                | Mode::Reacting { .. }
                | Mode::Unreacting { .. }
                | Mode::DateJump
                | Mode::SearchForm
                | Mode::SearchResults
        ) {
            self.clear_input_buffer();
            self.input.react_tab = None;
            self.pending_search = None;
            self.search_results = None;
            self.mode = Mode::Compose;
            // Back in compose: restore the room's draft into the buffer the
            // transient mode borrowed and cleared (M12).
            self.restore_draft_into_buffer();
        }
    }

    async fn handle_account_list_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Left && key.modifiers == KeyModifiers::ALT {
            self.adjust_accounts_width(-2);
        } else if key.code == KeyCode::Right && key.modifiers == KeyModifiers::ALT {
            self.adjust_accounts_width(2);
        } else if key.code == KeyCode::Up {
            self.accounts.selected = match self.accounts.selected {
                AccountSelection::All => AccountSelection::All,
                AccountSelection::Account(0) => AccountSelection::All,
                AccountSelection::Account(i) => AccountSelection::Account(i - 1),
            };
            self.sync_room_selection_to_account_filter();
            self.load_selected_timeline().await;
        } else if key.code == KeyCode::Down {
            self.accounts.selected = match self.accounts.selected {
                AccountSelection::All if self.accounts.accounts.is_empty() => AccountSelection::All,
                AccountSelection::All => AccountSelection::Account(0),
                AccountSelection::Account(i) => {
                    AccountSelection::Account((i + 1).min(self.accounts.accounts.len() - 1))
                }
            };
            self.sync_room_selection_to_account_filter();
            self.load_selected_timeline().await;
        } else if key.code == KeyCode::PageUp || self.shortcuts.message_page_up.matches(key) {
            let page = self.accounts.page_size.max(1) as isize;
            self.cycle_account(-page);
            self.load_selected_timeline().await;
        } else if key.code == KeyCode::PageDown || self.shortcuts.message_page_down.matches(key) {
            let page = self.accounts.page_size.max(1) as isize;
            self.cycle_account(page);
            self.load_selected_timeline().await;
        } else if key.code == KeyCode::Home {
            self.accounts.selected = AccountSelection::All;
            self.sync_room_selection_to_account_filter();
            self.load_selected_timeline().await;
        } else if key.code == KeyCode::End {
            let n = self.accounts.accounts.len();
            if n > 0 {
                self.accounts.selected = AccountSelection::Account(n - 1);
                self.sync_room_selection_to_account_filter();
                self.load_selected_timeline().await;
            }
        } else if self.shortcuts.find.matches(key) {
            self.accounts.scroll = 0;
            self.mode = Mode::Search(SearchKind::Accounts, String::new());
        } else if key.code == KeyCode::Char('/') && key.modifiers.is_empty() {
            self.clear_input_buffer();
            self.insert_char('/');
            self.mode = Mode::Compose;
        } else if key.code == KeyCode::Char('n') && key.modifiers.is_empty() {
            if let Some(q) = self.last_search.clone() {
                self.search_adjacent_account(&q, true);
                let search_status =
                    std::mem::replace(&mut self.status, Status::Info(String::new()));
                self.load_selected_timeline().await;
                self.status = search_status;
            }
        } else if key.code == KeyCode::Char('N') && key.modifiers == KeyModifiers::SHIFT {
            if let Some(q) = self.last_search.clone() {
                self.search_adjacent_account(&q, false);
                let search_status =
                    std::mem::replace(&mut self.status, Status::Info(String::new()));
                self.load_selected_timeline().await;
                self.status = search_status;
            }
        } else if self.shortcuts.submit.matches(key) || self.shortcuts.clear_input.matches(key) {
            self.clear_search_status();
            self.mode = Mode::Compose;
        }
    }

    fn cycle_focus(&mut self) {
        if matches!(
            self.mode,
            Mode::LoginUsername
                | Mode::LoginPassword { .. }
                | Mode::RecoveryKey { .. }
                | Mode::ConfirmLogout { .. }
                | Mode::ConfirmDelete { .. }
                | Mode::ConfirmRoomAction { .. }
                | Mode::Editing { .. }
                | Mode::Reacting { .. }
                | Mode::Unreacting { .. }
                | Mode::SearchForm
                | Mode::SearchResults
        ) {
            self.abandon_transient_input_mode();
            return;
        }
        let show_accounts = self.accounts_panel_visible();
        let show_rooms = self.rooms_panel_visible();
        let next = match (show_accounts, show_rooms) {
            (true, true) => match self.mode {
                Mode::Compose => Mode::AccountList,
                Mode::AccountList | Mode::Search(SearchKind::Accounts, _) => Mode::RoomList,
                Mode::RoomList | Mode::Search(SearchKind::Rooms, _) => Mode::MessageList,
                Mode::MessageList | Mode::Search(SearchKind::Messages, _) | Mode::Popup(_) => {
                    Mode::Compose
                }
                _ => Mode::Compose,
            },
            (true, false) => match self.mode {
                Mode::Compose => Mode::AccountList,
                Mode::AccountList | Mode::Search(SearchKind::Accounts, _) => Mode::MessageList,
                Mode::MessageList | Mode::Search(SearchKind::Messages, _) | Mode::Popup(_) => {
                    Mode::Compose
                }
                _ => Mode::Compose,
            },
            (false, true) => match self.mode {
                Mode::Compose => Mode::RoomList,
                Mode::RoomList | Mode::Search(SearchKind::Rooms, _) => Mode::MessageList,
                Mode::MessageList | Mode::Search(SearchKind::Messages, _) | Mode::Popup(_) => {
                    Mode::Compose
                }
                _ => Mode::Compose,
            },
            (false, false) => match self.mode {
                Mode::Compose | Mode::Popup(_) => Mode::MessageList,
                _ => Mode::Compose,
            },
        };
        match next {
            Mode::RoomList => self.rooms.scroll = 0,
            Mode::AccountList => self.accounts.scroll = 0,
            _ => {}
        }
        self.mode = next;
    }

    fn cycle_focus_prev(&mut self) {
        if matches!(
            self.mode,
            Mode::LoginUsername
                | Mode::LoginPassword { .. }
                | Mode::RecoveryKey { .. }
                | Mode::ConfirmLogout { .. }
                | Mode::ConfirmDelete { .. }
                | Mode::ConfirmRoomAction { .. }
                | Mode::Editing { .. }
                | Mode::Reacting { .. }
                | Mode::Unreacting { .. }
                | Mode::SearchForm
                | Mode::SearchResults
        ) {
            self.abandon_transient_input_mode();
            return;
        }
        let show_accounts = self.accounts_panel_visible();
        let show_rooms = self.rooms_panel_visible();
        let prev = match (show_accounts, show_rooms) {
            (true, true) => match self.mode {
                Mode::Compose => Mode::MessageList,
                Mode::AccountList | Mode::Search(SearchKind::Accounts, _) => Mode::Compose,
                Mode::RoomList | Mode::Search(SearchKind::Rooms, _) => Mode::AccountList,
                Mode::MessageList | Mode::Search(SearchKind::Messages, _) | Mode::Popup(_) => {
                    Mode::RoomList
                }
                _ => Mode::Compose,
            },
            (true, false) => match self.mode {
                Mode::Compose => Mode::MessageList,
                Mode::AccountList | Mode::Search(SearchKind::Accounts, _) => Mode::Compose,
                Mode::MessageList | Mode::Search(SearchKind::Messages, _) | Mode::Popup(_) => {
                    Mode::AccountList
                }
                _ => Mode::Compose,
            },
            (false, true) => match self.mode {
                Mode::Compose => Mode::MessageList,
                Mode::RoomList | Mode::Search(SearchKind::Rooms, _) => Mode::Compose,
                Mode::MessageList | Mode::Search(SearchKind::Messages, _) | Mode::Popup(_) => {
                    Mode::RoomList
                }
                _ => Mode::Compose,
            },
            (false, false) => match self.mode {
                Mode::Compose | Mode::Popup(_) => Mode::MessageList,
                _ => Mode::Compose,
            },
        };
        match prev {
            Mode::RoomList => self.rooms.scroll = 0,
            Mode::AccountList => self.accounts.scroll = 0,
            _ => {}
        }
        self.mode = prev;
    }

    /// True when the currently selected message is the first displayed event
    /// (used to decide whether Up-arrow should trigger a history load).
    fn is_at_first_message(&self) -> bool {
        let events = self.selected_events();
        match (events.first(), self.messages.selection.as_deref()) {
            (Some(first), Some(sel)) => first.event_id == sel,
            // No selection but events exist: treat as "at first" so Up triggers load.
            (Some(_), None) => true,
            _ => false,
        }
    }

    /// Whether the newest loaded message is the one selected — the trigger to
    /// page forward (load newer history) on Down / PageDown in a jumped view.
    fn is_at_last_message(&self) -> bool {
        let events = self.selected_events();
        match (events.last(), self.messages.selection.as_deref()) {
            (Some(last), Some(sel)) => last.event_id == sel,
            _ => false,
        }
    }

    /// Key handler for `Mode::DateJump`: the user is typing a date string into
    /// the compose buffer. Enter parses and jumps; Esc cancels.
    async fn handle_date_jump_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.clear_input_buffer();
                self.mode = Mode::MessageList;
                // Restore the room's draft into the buffer date entry borrowed
                // and cleared (M12); the exit lands in MessageList, not Compose.
                self.restore_draft_into_buffer();
                self.status = Status::Info("jump cancelled".to_owned());
            }
            KeyCode::Enter => {
                let text = self.input.buffer.trim().to_owned();
                match crate::command::parse_date_to_ms(&text) {
                    Some(ts) => {
                        self.mode = Mode::MessageList;
                        self.clear_input_buffer();
                        // Restore the room's draft into the buffer date entry
                        // borrowed and cleared (M12).
                        self.restore_draft_into_buffer();
                        self.jump_to_date(ts).await;
                    }
                    None => {
                        self.status = Status::Info(format!(
                            "unrecognized date \"{text}\" — try YYYY-MM-DD, M/D/YYYY, or \"Jan 15 2025\""
                        ));
                    }
                }
            }
            _ => {
                if !self.handle_input_navigation_key(key) {
                    if let KeyCode::Char(ch) = key.code {
                        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                            self.insert_char(ch);
                        }
                    }
                }
            }
        }
    }

    /// Jump to the previous (or next, when `forward`) calendar day that actually
    /// has messages, skipping over empty days. The landing day is centered in the
    /// pane so earlier and later messages stay visible around it (ADR 0038).
    async fn jump_adjacent_day(&mut self, forward: bool) {
        const DAY_MS: i64 = 86_400_000;
        let anchor = self.jump_day_anchor();
        let day_start = anchor.div_euclid(DAY_MS) * DAY_MS;

        // Find the UTC day-start of the adjacent day that actually has messages,
        // then let `jump_to_date` center on it (and fill later messages below).
        let target_day_start = if forward {
            let lower = day_start + DAY_MS;
            match self.find_first_event_at_or_after(lower).await {
                Some(first_ts) => first_ts.div_euclid(DAY_MS) * DAY_MS,
                None => {
                    self.status = Status::Info("no later day with messages".to_owned());
                    return;
                }
            }
        } else {
            match self.last_event_ts_at_or_before(day_start - 1).await {
                Some(last_ts) => last_ts.div_euclid(DAY_MS) * DAY_MS,
                None => {
                    self.status = Status::Info("no earlier day with messages".to_owned());
                    return;
                }
            }
        };
        self.jump_to_date(target_day_start).await;
    }

    /// Enter DateJump mode (the `Shift+J` "jump to a date" prompt).
    fn start_date_jump(&mut self) {
        // Settle the room's draft before the buffer is borrowed for date entry,
        // so returning restores it instead of tombstoning it (M12).
        self.flush_pending_draft_now();
        self.clear_input_buffer();
        self.status =
            Status::Info("Enter date (YYYY-MM-DD, M/D/YYYY, Jan 15 2025): Esc cancels".to_owned());
        self.mode = Mode::DateJump;
    }

    /// Anchor timestamp for ±1-day jumps.
    ///
    /// Prefers the explicitly tracked jump timestamp so repeated Shift-PgUp/Dn
    /// steps each exactly one calendar day rather than drifting based on which
    /// message happens to be at the edge of the loaded page.  Falls back to the
    /// selected message, then the newest visible message, then now.
    fn jump_day_anchor(&self) -> i64 {
        self.last_jump_ts
            .or_else(|| self.selected_message_event().map(|e| e.origin_ts))
            .or_else(|| {
                self.selected_events()
                    .into_iter()
                    .last()
                    .map(|e| e.origin_ts)
            })
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis())
    }
}
