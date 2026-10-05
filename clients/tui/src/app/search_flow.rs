use uuid::Uuid;

use crate::api::{
    ApiError, AxonClient, EventDto, RoomDto, SearchPage, SearchResultDto, TimelinePage,
};
use crate::search::{
    parse_search_terms, ParsedSearch, SearchCommandInput, SearchContextKey, SearchFormState,
    SearchGrouping, SearchRequest, SearchResultContext, SearchResultsState, SearchScope,
    DEFAULT_CONTEXT_RADIUS, SEARCH_HELP_TEXT,
};

use super::{
    account_localpart, apply_edits, rooms::fetch_straddling_timeline_page, App, Mode, RoomKey,
    RoomTargetResolution, Status, PENDING_ECHO_MSG, TIMELINE_LIMIT,
};

const SEARCH_CONTEXT_LIMIT: usize = 25;

#[derive(Debug)]
pub(crate) enum SearchOutcome {
    Initial {
        generation: u64,
        grouping: SearchGrouping,
        request: SearchRequest,
        edit_form: SearchFormState,
        result: Result<SearchPage, ApiError>,
    },
    Page {
        generation: u64,
        request: SearchRequest,
        cursor: String,
        result: Result<SearchPage, ApiError>,
    },
    Context {
        key: SearchContextKey,
        hit: EventDto,
        result: Result<TimelinePage, ApiError>,
    },
    Jump {
        hit: EventDto,
        action: SearchJumpAction,
        room_refresh: Option<Result<Vec<RoomDto>, ApiError>>,
        result: Result<TimelinePage, ApiError>,
        thread_load: Option<Box<SearchJumpThreadLoad>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchJumpAction {
    View,
    Reply,
    Thread,
}

#[derive(Debug)]
pub(crate) struct SearchJumpThreadLoad {
    pub(crate) timeline: Result<TimelinePage, ApiError>,
    pub(crate) root_event: Result<EventDto, ApiError>,
}

impl App {
    pub(crate) async fn open_or_run_search(&mut self, input: SearchCommandInput) {
        match input {
            SearchCommandInput::OpenForm => self.open_search_form(),
            SearchCommandInput::Help => self.status = Status::from(SEARCH_HELP_TEXT),
            SearchCommandInput::Run(raw) => match parse_search_terms(&raw) {
                Ok(parsed) => self.run_parsed_search(parsed).await,
                Err(err) => self.status = Status::from(err),
            },
        }
    }

    pub(crate) fn open_search_form(&mut self) {
        self.search_form = SearchFormState::default();
        self.search_form.query.clear();
        self.search_form.error = None;
        self.mode = Mode::SearchForm;
        self.status = Status::from("search form".to_owned());
    }

    pub(crate) async fn submit_search_form(&mut self) {
        match self.search_form.to_parsed() {
            Ok(mut parsed) => {
                if self.search_form.scope == SearchScope::CurrentAccount {
                    match self.current_search_form_account() {
                        Ok(account_id) => parsed.account = Some(account_id.to_string()),
                        Err(err) => {
                            self.search_form.error = Some(err.clone());
                            self.status = Status::from(err);
                            self.mode = Mode::SearchForm;
                            return;
                        }
                    }
                }
                let edit_form = self.search_form.clone();
                self.run_parsed_search_with_form(parsed, edit_form).await;
            }
            Err(err) => {
                self.search_form.error = Some(err.clone());
                self.status = Status::from(err);
                self.mode = Mode::SearchForm;
            }
        }
    }

    pub(crate) async fn run_parsed_search(&mut self, parsed: ParsedSearch) {
        let edit_form = SearchFormState::from_parsed(&parsed);
        self.run_parsed_search_with_form(parsed, edit_form).await;
    }

    async fn run_parsed_search_with_form(
        &mut self,
        parsed: ParsedSearch,
        edit_form: SearchFormState,
    ) {
        let request = match self.search_request_from(parsed) {
            Ok(request) => request,
            Err(err) => {
                self.status = Status::from(err);
                return;
            }
        };
        if request.q.trim().is_empty() && !request.has_narrowing_filter() {
            self.status = Status::from("search requires text or at least one filter".to_owned());
            return;
        }
        self.start_search_request(request, edit_form, SearchGrouping::None);
    }

    fn start_search_request(
        &mut self,
        request: SearchRequest,
        edit_form: SearchFormState,
        grouping: SearchGrouping,
    ) {
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        self.status = Status::from(searching_status(&request));
        self.pending_search = Some(request.clone());
        let Some(tx) = self.search_tx.clone() else {
            self.pending_search = None;
            self.status = Status::from("search unavailable: background channel is not wired");
            return;
        };
        let client = self.client.clone();
        tokio::spawn(async move {
            let result = client.search(&request).await;
            let _ = tx.send(SearchOutcome::Initial {
                generation,
                grouping,
                request,
                edit_form,
                result,
            });
        });
    }

    pub(crate) fn handle_search_outcome(&mut self, outcome: SearchOutcome) {
        match outcome {
            SearchOutcome::Initial {
                generation,
                grouping,
                request,
                edit_form,
                result,
            } => {
                self.apply_initial_search_outcome(generation, request, edit_form, grouping, result)
            }
            SearchOutcome::Page {
                generation,
                request,
                cursor,
                result,
            } => self.apply_search_page_outcome(generation, request, &cursor, result),
            SearchOutcome::Context { key, hit, result } => {
                self.apply_search_context_outcome(key, hit, result);
            }
            SearchOutcome::Jump {
                hit,
                action,
                room_refresh,
                result,
                thread_load,
            } => self.apply_search_jump_outcome(hit, action, room_refresh, result, thread_load),
        }
    }

    fn current_search_form_account(&self) -> Result<Uuid, String> {
        self.active_account_filter()
            .or_else(|| {
                (self.accounts.accounts.len() == 1).then(|| self.accounts.accounts[0].account_id)
            })
            .ok_or_else(|| "select an account or choose all accounts".to_owned())
    }

    fn apply_initial_search_outcome(
        &mut self,
        generation: u64,
        request: SearchRequest,
        edit_form: SearchFormState,
        grouping: SearchGrouping,
        result: Result<SearchPage, ApiError>,
    ) {
        if generation != self.search_generation {
            return;
        }
        if self.pending_search.as_ref() != Some(&request) {
            return;
        }
        self.pending_search = None;
        // A grouping toggle while a new sort is pending should survive its
        // response. New queries still use their requested initial grouping.
        let grouping = self
            .search_results
            .as_ref()
            .filter(|state| {
                let mut previous = state.request.clone();
                previous.sort = request.sort;
                previous == request
            })
            .map_or(grouping, |state| state.grouping);
        match result {
            Ok(page) => {
                let total = page.total;
                let count = page.results.len();
                let mut state = SearchResultsState {
                    request,
                    edit_form,
                    results: page.results,
                    total,
                    next_cursor: page.next_cursor,
                    selected: 0,
                    loading: false,
                    grouping,
                    context_cache: Default::default(),
                };
                state.select_first_ordered();
                self.search_results = Some(state);
                self.mode = Mode::SearchResults;
                self.status = Status::from(if count == 0 {
                    "search: no results".to_owned()
                } else {
                    format!("search: showing {count} of {total}")
                });
                self.request_selected_search_context();
            }
            Err(err) => self.status = Status::from(search_error_status(&err)),
        }
    }

    pub(crate) fn edit_current_search(&mut self) {
        let Some(state) = self.search_results.as_ref() else {
            return;
        };
        self.search_form = state.edit_form.clone();
        self.search_form.field = crate::search::SearchFormField::Query;
        self.search_form.error = None;
        self.mode = Mode::SearchForm;
        self.status = Status::from("edit search".to_owned());
    }

    fn search_request_from(&self, parsed: ParsedSearch) -> Result<SearchRequest, String> {
        let (account_id, room_id) = if parsed.all {
            (None, None)
        } else if let Some(room_target) = parsed.room.as_deref() {
            if is_all_rooms_target(room_target) {
                (
                    self.resolve_account_filter(parsed.account.as_deref())?,
                    None,
                )
            } else {
                let account_filter = if parsed.account.is_some() {
                    self.resolve_account_filter(parsed.account.as_deref())?
                } else {
                    self.active_account_filter()
                };
                let index = self.resolve_search_room(room_target, account_filter)?;
                let room = &self.rooms.rooms[index];
                (Some(room.account_id), Some(room.room_id.clone()))
            }
        } else if parsed.account.is_some() {
            (
                self.resolve_account_filter(parsed.account.as_deref())?,
                None,
            )
        } else if let Some(room) = self.selected_room() {
            (Some(room.account_id), Some(room.room_id.clone()))
        } else {
            return Err("select a room or use account:* before searching".to_owned());
        };

        Ok(SearchRequest {
            q: parsed.query,
            account_id,
            room_id,
            sender: parsed.sender,
            from: parsed.from,
            to: parsed.to,
            limit: parsed.limit.clamp(1, 200),
            sort: crate::search::SearchSortOrder::NewestFirst,
            cursor: None,
        })
    }

    fn resolve_search_room(
        &self,
        target: &str,
        account_filter: Option<Uuid>,
    ) -> Result<usize, String> {
        match self.resolve_room_target_in_account(target, account_filter) {
            RoomTargetResolution::Match(index) => Ok(index),
            RoomTargetResolution::Ambiguous(options) => {
                Err(format!("room name is ambiguous: {}", options.join(", ")))
            }
            RoomTargetResolution::Missing => Err(format!("room not found: {target}")),
        }
    }

    fn resolve_account_filter(&self, target: Option<&str>) -> Result<Option<Uuid>, String> {
        let Some(target) = target.filter(|target| !target.trim().is_empty()) else {
            return self
                .active_account_filter()
                .or_else(|| {
                    (self.accounts.accounts.len() == 1)
                        .then(|| self.accounts.accounts[0].account_id)
                })
                .map(Some)
                .ok_or_else(|| {
                    "select an account or use account:* before searching all rooms".to_owned()
                });
        };
        if target == "*" || target.eq_ignore_ascii_case("all") || target == "0" {
            return Ok(None);
        }
        if let Ok(n) = target.parse::<usize>() {
            return n
                .checked_sub(1)
                .and_then(|idx| self.accounts.accounts.get(idx))
                .map(|account| Some(account.account_id))
                .ok_or_else(|| format!("account index out of range: {target}"));
        }
        if let Ok(account_id) = Uuid::parse_str(target) {
            if self
                .accounts
                .accounts
                .iter()
                .any(|account| account.account_id == account_id)
            {
                return Ok(Some(account_id));
            }
        }
        let target_lower = target.to_ascii_lowercase();
        let local = target.trim_start_matches('@');
        let matches: Vec<_> = self
            .accounts
            .accounts
            .iter()
            .filter(|account| {
                account.user_id.eq_ignore_ascii_case(target)
                    || account.user_id.to_ascii_lowercase().contains(&target_lower)
                    || account_localpart(&account.user_id).is_some_and(|part| part == local)
            })
            .collect();
        match matches.as_slice() {
            [account] => Ok(Some(account.account_id)),
            [] => Err(format!("account not found: {target}")),
            many => Err(format!(
                "account is ambiguous: {}",
                many.iter()
                    .map(|account| account.user_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    pub(crate) async fn move_search_result_selection(&mut self, delta: isize) {
        let Some(state) = self.search_results.as_mut() else {
            return;
        };
        if state.results.is_empty() {
            return;
        }
        let ordered = state.ordered_indices();
        let position = ordered
            .iter()
            .position(|index| *index == state.selected)
            .unwrap_or(0);
        let next_position = if delta.is_negative() {
            position.saturating_sub(delta.unsigned_abs())
        } else {
            (position + delta as usize).min(ordered.len() - 1)
        };
        state.selected = ordered[next_position];
        self.request_selected_search_context();
        self.prefetch_more_search_results_if_needed();
    }

    pub(crate) async fn page_search_result_selection(&mut self, direction: isize) {
        let page = self.messages.page_size.max(5) as isize;
        self.move_search_result_selection(direction.saturating_mul(page))
            .await;
    }

    pub(crate) async fn jump_to_search_result_edge(&mut self, last: bool) {
        let Some(state) = self.search_results.as_mut() else {
            return;
        };
        let ordered = state.ordered_indices();
        let edge = if last {
            ordered.last().copied()
        } else {
            ordered.first().copied()
        };
        let Some(index) = edge else {
            return;
        };
        state.selected = index;
        self.request_selected_search_context();
        self.prefetch_more_search_results_if_needed();
    }

    pub(crate) async fn toggle_search_result_sort_order(&mut self) {
        let Some(state) = self.search_results.as_mut() else {
            return;
        };
        let mut request = state.request.clone();
        request.sort = self
            .pending_search
            .as_ref()
            .map_or(request.sort, |pending| pending.sort)
            .toggle();
        request.cursor = None;
        let edit_form = state.edit_form.clone();
        let grouping = state.grouping;
        // Supersede any page fetch while retaining the old results if the new
        // ordering fails. Its response is guarded by the search generation.
        state.loading = false;
        self.start_search_request(request, edit_form, grouping);
    }

    pub(crate) async fn toggle_search_result_grouping(&mut self) {
        let Some(state) = self.search_results.as_mut() else {
            return;
        };
        state.grouping = state.grouping.toggle();
        state.select_first_ordered();
        let label = state.grouping.label();
        self.status = Status::from(format!("search: grouped by {label}"));
        self.request_selected_search_context();
    }

    fn prefetch_more_search_results_if_needed(&mut self) {
        let should_fetch = self.search_results.as_ref().is_some_and(|state| {
            let ordered_len = state.results.len();
            let selected_position = state.selected_order_position().unwrap_or(0);
            state.next_cursor.is_some()
                && !state.loading
                && selected_position.saturating_add(5) >= ordered_len
        });
        if should_fetch {
            self.fetch_next_search_page();
        }
    }

    pub(crate) fn fetch_next_search_page(&mut self) {
        if self.pending_search.is_some() {
            return;
        }
        let generation = self.search_generation;
        let Some((base_request, cursor)) = self.search_results.as_mut().and_then(|state| {
            if state.loading {
                None
            } else {
                state
                    .next_cursor
                    .clone()
                    .map(|cursor| (state.request.clone(), cursor))
            }
        }) else {
            return;
        };
        let mut request = base_request.clone();
        request.cursor = Some(cursor);
        if let Some(state) = self.search_results.as_mut() {
            state.loading = true;
        }
        let Some(tx) = self.search_tx.clone() else {
            if let Some(state) = self.search_results.as_mut() {
                state.loading = false;
            }
            self.status =
                Status::from("search pagination unavailable: background channel is not wired");
            return;
        };
        let client = self.client.clone();
        let cursor = request.cursor.clone().unwrap_or_default();
        tokio::spawn(async move {
            let result = client.search(&request).await;
            let _ = tx.send(SearchOutcome::Page {
                generation,
                request: base_request,
                cursor,
                result,
            });
        });
    }

    fn apply_search_page_outcome(
        &mut self,
        generation: u64,
        request: SearchRequest,
        cursor: &str,
        result: Result<SearchPage, ApiError>,
    ) {
        if generation != self.search_generation || self.pending_search.is_some() {
            return;
        }
        let Some(state) = self.search_results.as_mut() else {
            return;
        };
        if state.request != request {
            return;
        }
        if state.next_cursor.as_deref() != Some(cursor) {
            state.loading = false;
            return;
        }
        state.loading = false;
        match result {
            Ok(page) => {
                state.results.extend(page.results);
                state.total = page.total;
                state.next_cursor = page.next_cursor;
            }
            Err(err) => self.status = Status::from(format!("search page failed: {err}")),
        }
    }

    pub(crate) fn request_selected_search_context(&mut self) {
        let Some(result) = self.selected_search_result().cloned() else {
            return;
        };
        let key = SearchContextKey::from_event(&result.event);
        if self
            .search_results
            .as_ref()
            .is_some_and(|state| state.context_cache.contains_key(&key))
        {
            return;
        }
        let Some(tx) = self.search_tx.clone() else {
            self.insert_search_context(key, result.event.clone(), Vec::new());
            return;
        };
        let client = self.client.clone();
        let hit = result.event.clone();
        tokio::spawn(async move {
            let result = fetch_straddling_timeline_page(
                client,
                hit.account_id,
                hit.room_id.clone(),
                hit.origin_ts,
                SEARCH_CONTEXT_LIMIT,
                DEFAULT_CONTEXT_RADIUS,
            )
            .await;
            let _ = tx.send(SearchOutcome::Context { key, hit, result });
        });
    }

    fn apply_search_context_outcome(
        &mut self,
        key: SearchContextKey,
        hit: EventDto,
        result: Result<TimelinePage, ApiError>,
    ) {
        let context_is_current = self.search_results.as_ref().is_some_and(|state| {
            state
                .results
                .iter()
                .any(|result| SearchContextKey::from_event(&result.event) == key)
        });
        if !context_is_current {
            return;
        }
        let events = match result {
            Ok(mut page) => {
                page.events.reverse();
                page.events
            }
            Err(err) => {
                if self.mode == Mode::SearchResults {
                    self.status = Status::from(format!("search context failed: {err}"));
                }
                Vec::new()
            }
        };
        self.insert_search_context(key, hit, events);
    }

    fn insert_search_context(
        &mut self,
        key: SearchContextKey,
        hit: EventDto,
        mut events: Vec<EventDto>,
    ) {
        let Some(state) = self.search_results.as_mut() else {
            return;
        };
        if !state
            .results
            .iter()
            .any(|result| SearchContextKey::from_event(&result.event) == key)
        {
            return;
        }
        merge_hit_into_events(&mut events, &hit);
        apply_edits(&mut events);
        let hit_index = events
            .iter()
            .position(|event| event.event_id == hit.event_id)
            .unwrap_or(0);
        let start = hit_index.saturating_sub(DEFAULT_CONTEXT_RADIUS);
        let end = (hit_index + DEFAULT_CONTEXT_RADIUS + 1).min(events.len());
        let context = SearchResultContext {
            events: events[start..end].to_vec(),
        };
        state.context_cache.insert(key, context);
    }

    pub(crate) fn selected_search_result(&self) -> Option<&SearchResultDto> {
        let state = self.search_results.as_ref()?;
        state.results.get(state.selected)
    }

    pub(crate) async fn jump_to_selected_search_result(&mut self) -> bool {
        self.start_search_result_jump(SearchJumpAction::View)
    }

    pub(crate) async fn reply_to_selected_search_result(&mut self) {
        self.start_search_result_jump(SearchJumpAction::Reply);
    }

    pub(crate) async fn thread_from_selected_search_result(&mut self) {
        self.start_search_result_jump(SearchJumpAction::Thread);
    }

    fn start_search_result_jump(&mut self, action: SearchJumpAction) -> bool {
        let Some(result) = self.selected_search_result().cloned() else {
            self.status = Status::from("search: no result selected".to_owned());
            return false;
        };
        let hit = result.event;
        let Some(tx) = self.search_tx.clone() else {
            self.status = Status::from("search jump unavailable: background channel is not wired");
            return false;
        };
        self.status = Status::from("loading search result...".to_owned());
        let client = self.client.clone();
        let needs_room_refresh = self.find_room_index(hit.account_id, &hit.room_id).is_none();
        let account_filter = self.account_filter;
        let want_after = self.messages.page_size / 2;
        let account_id = hit.account_id;
        let room_id = hit.room_id.clone();
        let origin_ts = hit.origin_ts;
        let thread_root = hit.thread_relation().map(str::to_owned);
        tokio::spawn(async move {
            let room_refresh = if needs_room_refresh {
                Some(client.list_rooms(account_filter).await)
            } else {
                None
            };
            let timeline_client = client.clone();
            let thread_client = client.clone();
            let timeline = fetch_straddling_timeline_page(
                timeline_client,
                account_id,
                room_id.clone(),
                origin_ts,
                TIMELINE_LIMIT,
                want_after,
            );
            let thread = async move {
                match thread_root {
                    Some(root) => Some(
                        fetch_search_jump_thread(thread_client, account_id, room_id, root).await,
                    ),
                    None => None,
                }
            };
            let (result, thread_load) = tokio::join!(timeline, thread);
            let _ = tx.send(SearchOutcome::Jump {
                hit,
                action,
                room_refresh,
                result,
                thread_load: thread_load.map(Box::new),
            });
        });
        true
    }

    fn apply_search_jump_outcome(
        &mut self,
        hit: EventDto,
        action: SearchJumpAction,
        room_refresh: Option<Result<Vec<RoomDto>, ApiError>>,
        result: Result<TimelinePage, ApiError>,
        thread_load: Option<Box<SearchJumpThreadLoad>>,
    ) {
        if let Some(room_refresh) = room_refresh {
            match room_refresh {
                Ok(rooms) => self.apply_room_refresh(rooms),
                Err(err) => {
                    self.status = Status::from(format!("room refresh failed: {err}"));
                    return;
                }
            }
        }
        let Some(room_index) = self.find_room_index(hit.account_id, &hit.room_id) else {
            self.status = Status::from("search result room is not visible".to_owned());
            return;
        };
        self.select_room_account(room_index);
        self.prepare_room_selection(room_index);
        self.apply_timeline_around_search_event(hit, result, thread_load);
        self.search_results = None;
        self.mode = Mode::MessageList;
        match action {
            SearchJumpAction::View => {}
            SearchJumpAction::Reply => self.start_reply_to_selected_message(),
            SearchJumpAction::Thread => self.start_thread_from_loaded_search_result(),
        }
    }

    fn apply_timeline_around_search_event(
        &mut self,
        hit: EventDto,
        result: Result<TimelinePage, ApiError>,
        thread_load: Option<Box<SearchJumpThreadLoad>>,
    ) {
        let Some(room) = self.selected_room().cloned() else {
            return;
        };
        let key = RoomKey::from(&room);
        let load_error = result.as_ref().err().map(ToString::to_string);
        let mut page = result.unwrap_or_else(|_| TimelinePage {
            events: vec![hit.clone()],
            next_cursor: None,
        });
        page.events.reverse();
        if let Some(thread_load) = thread_load {
            merge_search_jump_thread_load(&mut page.events, *thread_load);
        }
        merge_hit_into_events(&mut page.events, &hit);
        apply_edits(&mut page.events);
        self.rebuild_display_names(&room, &page.events);
        match page.next_cursor.take() {
            Some(cursor) => {
                self.messages.history_cursors.insert(key.clone(), cursor);
            }
            None => {
                self.messages.history_cursors.remove(&key);
            }
        }
        let selected_index = page
            .events
            .iter()
            .position(|event| event.event_id == hit.event_id)
            .unwrap_or(0);
        self.messages.events.insert(key.clone(), page.events);
        self.rooms.unread.remove(&key);
        self.last_jump_ts = Some(hit.origin_ts);
        self.thread_panel = None;
        self.messages.selection = Some(hit.event_id.clone());
        self.ensure_message_index_visible(selected_index);
        if let Some(root) = hit.thread_relation() {
            self.open_loaded_thread_panel(root.to_owned(), false);
            self.messages.selection = Some(hit.event_id.clone());
            if let Some(index) = self
                .selected_events()
                .iter()
                .position(|event| event.event_id == hit.event_id)
            {
                self.ensure_message_index_visible(index);
            }
        }
        self.status = if let Some(err) = load_error {
            Status::from(format!(
                "jumped to cached search result in {} (timeline load failed: {err})",
                room.title()
            ))
        } else {
            Status::from(format!("jumped to search result in {}", room.title()))
        };
    }

    fn start_thread_from_loaded_search_result(&mut self) {
        let Some(event) = self.selected_message_event().cloned() else {
            self.status =
                Status::from("select a displayed message before opening a thread".to_owned());
            return;
        };
        if event.event_id.starts_with("local-echo:") {
            self.status = Status::from(PENDING_ECHO_MSG.to_owned());
            return;
        }
        if let Some(root) = event.thread_relation().map(str::to_owned) {
            self.open_loaded_thread_panel(root, true);
        } else if self.is_thread_root(&event.event_id) {
            self.open_loaded_thread_panel(event.event_id, true);
        } else {
            self.pending_reply = None;
            self.pending_thread = Some(event.event_id.clone());
            self.mode = Mode::Compose;
            self.status = Status::EventAction {
                debug: format!(
                    "replying in new thread on {} - Esc to cancel",
                    event.event_id
                ),
                redacted: "replying in new thread - Esc to cancel",
            };
        }
    }

    fn open_loaded_thread_panel(&mut self, root: String, focus_compose: bool) {
        let Some(room) = self.selected_room().cloned() else {
            return;
        };
        let key = RoomKey::from(&room);
        let panel_members: Vec<String> = self
            .messages
            .events
            .get(&key)
            .map(|events| {
                events
                    .iter()
                    .filter(|event| event.thread_relation() == Some(root.as_str()))
                    .map(|event| event.event_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        for id in panel_members {
            self.promoted_thread_events.remove(&id);
        }
        self.thread_panel = Some(root);
        self.force_terminal_clear = true;
        let count = self.selected_events().len();
        if count > 0 {
            let selected_index = self
                .messages
                .selection
                .as_ref()
                .and_then(|selected| {
                    self.selected_events()
                        .iter()
                        .position(|event| event.event_id == *selected)
                })
                .unwrap_or_else(|| count.saturating_sub(1));
            self.ensure_message_index_visible(selected_index);
        } else {
            self.messages.scroll = usize::MAX;
        }
        if focus_compose {
            self.mode = Mode::Compose;
            self.status = Status::Info(format!(
                "[in thread] {} reply(ies) - Esc to exit",
                count.saturating_sub(1)
            ));
        }
    }

    fn find_room_index(&self, account_id: Uuid, room_id: &str) -> Option<usize> {
        self.rooms
            .rooms
            .iter()
            .position(|room| room.account_id == account_id && room.room_id == room_id)
    }

    pub(crate) fn search_result_status(&self) -> String {
        let Some(state) = self.search_results.as_ref() else {
            return String::new();
        };
        if state.results.is_empty() {
            "no results".to_owned()
        } else {
            let selected = state.selected_order_position().unwrap_or(0) + 1;
            format!(
                "result {} of {} loaded ({} total)",
                selected,
                state.results.len(),
                state.total
            )
        }
    }

    pub(crate) fn clear_search_results(&mut self) {
        self.pending_search = None;
        self.search_results = None;
        self.mode = Mode::Compose;
        self.status = Status::from(String::new());
    }

    pub(crate) fn search_form_insert(&mut self, ch: char) {
        if let Some(buffer) = self.search_form.edit_buffer_mut() {
            buffer.push(ch);
        }
        self.search_form.error = None;
    }

    pub(crate) fn search_form_backspace(&mut self) {
        if let Some(buffer) = self.search_form.edit_buffer_mut() {
            buffer.pop();
        }
        self.search_form.error = None;
    }
}

fn searching_status(request: &SearchRequest) -> String {
    if request.q.trim().is_empty() {
        "searching filtered messages...".to_owned()
    } else {
        format!("searching for \"{}\"...", request.q)
    }
}

fn search_error_status(err: &ApiError) -> String {
    if err.is_not_found() {
        "search is not supported by this Axon server".to_owned()
    } else if err.is_service_unavailable() {
        "search is disabled on this Axon server".to_owned()
    } else {
        format!("search failed: {err}")
    }
}

async fn fetch_search_jump_thread(
    client: AxonClient,
    account_id: Uuid,
    room_id: String,
    root: String,
) -> SearchJumpThreadLoad {
    let timeline = client
        .thread_timeline(account_id, &room_id, &root, None, TIMELINE_LIMIT)
        .await;
    let root_event = client.get_event(account_id, &root).await;
    SearchJumpThreadLoad {
        timeline,
        root_event,
    }
}

fn merge_search_jump_thread_load(events: &mut Vec<EventDto>, load: SearchJumpThreadLoad) {
    if let Ok(mut page) = load.timeline {
        page.events.reverse();
        merge_events_into(events, page.events);
    }
    if let Ok(root) = load.root_event {
        merge_events_into(events, [root]);
    }
}

fn merge_events_into(events: &mut Vec<EventDto>, incoming: impl IntoIterator<Item = EventDto>) {
    for event in incoming {
        if !events
            .iter()
            .any(|existing| existing.event_id == event.event_id)
        {
            events.push(event);
        }
    }
    events.sort_by_key(|event| (event.origin_ts, event.event_id.clone()));
}

fn merge_hit_into_events(events: &mut Vec<EventDto>, hit: &EventDto) {
    merge_events_into(events, [hit.clone()]);
}

impl SearchRequest {
    fn has_narrowing_filter(&self) -> bool {
        self.account_id.is_some()
            || self
                .room_id
                .as_deref()
                .is_some_and(|room_id| !room_id.trim().is_empty())
            || self
                .sender
                .as_deref()
                .is_some_and(|sender| !sender.trim().is_empty())
            || self.from.is_some()
            || self.to.is_some()
    }
}

fn is_all_rooms_target(target: &str) -> bool {
    let target = target.trim();
    target.is_empty() || target == "*"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> SearchRequest {
        SearchRequest {
            q: String::new(),
            account_id: None,
            room_id: None,
            sender: None,
            from: None,
            to: None,
            limit: crate::search::DEFAULT_SEARCH_LIMIT,
            sort: crate::search::SearchSortOrder::NewestFirst,
            cursor: None,
        }
    }

    fn test_app() -> App {
        App::new(
            AxonClient::new("http://127.0.0.1:1".to_owned(), None),
            None,
            crate::config::TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        )
    }

    fn page(id: &str, cursor: Option<&str>) -> SearchPage {
        serde_json::from_value(serde_json::json!({
            "results": [{ "score": 0.1, "event": {
                "account_id": Uuid::nil(), "event_id": id, "room_id": "!r:x",
                "sender": "@alice:x", "origin_ts": 1000, "arrival_order": 1000,
                "type": "m.room.message", "body": "needle", "redacted": false
            }}], "total": 200, "next_cursor": cursor
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn sort_restarts_pagination_preserves_grouping_and_rejects_stale_outcomes() {
        use crate::search::SearchSortOrder;
        let mut app = test_app();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.search_tx = Some(tx);
        let mut initial = request();
        initial.q = "needle".to_owned();
        let form = SearchFormState::from_parsed(&parse_search_terms("needle").unwrap());
        app.start_search_request(initial.clone(), form.clone(), SearchGrouping::Room);
        let first_generation = app.search_generation;
        app.apply_initial_search_outcome(
            first_generation,
            initial.clone(),
            form.clone(),
            SearchGrouping::Room,
            Ok(page("$newest", Some("newest-cursor"))),
        );
        app.search_results.as_mut().unwrap().loading = true;

        app.toggle_search_result_sort_order().await;
        let oldest = app.pending_search.clone().unwrap();
        assert_eq!(oldest.sort, SearchSortOrder::OldestFirst);
        assert!(oldest.cursor.is_none());
        app.apply_search_page_outcome(
            first_generation,
            initial.clone(),
            "newest-cursor",
            Ok(page("$stale-page", None)),
        );
        assert_eq!(app.search_results.as_ref().unwrap().results.len(), 1);

        // Toggle back before the oldest response arrives. The request is again
        // identical to the original, so request equality alone is insufficient.
        app.toggle_search_result_sort_order().await;
        assert_eq!(app.pending_search.as_ref(), Some(&initial));
        app.apply_initial_search_outcome(
            first_generation,
            initial.clone(),
            form.clone(),
            SearchGrouping::None,
            Ok(page("$stale-initial", None)),
        );
        assert_eq!(
            app.search_results.as_ref().unwrap().results[0]
                .event
                .event_id,
            "$newest"
        );
        app.apply_initial_search_outcome(
            app.search_generation,
            initial.clone(),
            form.clone(),
            SearchGrouping::Room,
            Ok(page("$fresh", Some("fresh-cursor"))),
        );
        let state = app.search_results.as_ref().unwrap();
        assert_eq!(state.request.sort, SearchSortOrder::NewestFirst);
        assert_eq!(state.grouping, SearchGrouping::Room);
        assert_eq!(state.next_cursor.as_deref(), Some("fresh-cursor"));
        assert_eq!(state.selected, 0);
        app.apply_search_page_outcome(
            app.search_generation,
            initial,
            "fresh-cursor",
            Ok(page("$fresh-page", None)),
        );
        assert_eq!(app.search_results.as_ref().unwrap().results.len(), 2);

        app.toggle_search_result_sort_order().await;
        let pending = app.pending_search.clone().unwrap();
        app.clear_search_results();
        app.apply_initial_search_outcome(
            app.search_generation,
            pending,
            form,
            SearchGrouping::Room,
            Ok(page("$closed", None)),
        );
        assert!(app.search_results.is_none());
        assert_eq!(app.mode, Mode::Compose);
    }

    #[tokio::test]
    async fn failed_sort_keeps_previous_results_and_grouping_changes_survive_pending_sort() {
        let mut app = test_app();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.search_tx = Some(tx);
        let initial = app
            .search_request_from(parse_search_terms("all:true needle").unwrap())
            .unwrap();
        assert_eq!(initial.sort, crate::search::SearchSortOrder::NewestFirst);
        let form = SearchFormState::from_parsed(&parse_search_terms("needle").unwrap());
        app.start_search_request(initial.clone(), form.clone(), SearchGrouping::None);
        app.apply_initial_search_outcome(
            app.search_generation,
            initial,
            form.clone(),
            SearchGrouping::None,
            Ok(page("$kept", Some("kept-cursor"))),
        );
        app.toggle_search_result_sort_order().await;
        let oldest = app.pending_search.clone().unwrap();
        app.apply_initial_search_outcome(
            app.search_generation,
            oldest,
            form.clone(),
            SearchGrouping::None,
            Err(ApiError::Status {
                status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                message: "unavailable".to_owned(),
            }),
        );
        let state = app.search_results.as_ref().unwrap();
        assert_eq!(state.results[0].event.event_id, "$kept");
        assert_eq!(state.next_cursor.as_deref(), Some("kept-cursor"));
        assert_eq!(
            state.request.sort,
            crate::search::SearchSortOrder::NewestFirst
        );
        assert!(!state.loading);
        app.toggle_search_result_sort_order().await;
        let oldest = app.pending_search.clone().unwrap();
        app.toggle_search_result_grouping().await;
        app.apply_initial_search_outcome(
            app.search_generation,
            oldest,
            form,
            SearchGrouping::None,
            Ok(page("$oldest", None)),
        );
        assert_eq!(
            app.search_results.as_ref().unwrap().grouping,
            SearchGrouping::Room
        );
    }

    #[tokio::test]
    async fn search_sends_requested_order_on_initial_and_cursor_pages() {
        use crate::search::SearchSortOrder;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut targets = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 1024];
                    let read = socket.read(&mut chunk).await.unwrap();
                    assert!(read > 0);
                    bytes.extend_from_slice(&chunk[..read]);
                    if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                targets.push(
                    String::from_utf8(bytes)
                        .unwrap()
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .to_owned(),
                );
                let body = r#"{"data":{"results":[],"total":0,"next_cursor":null}}"#;
                socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            targets
        });
        let client = AxonClient::new(format!("http://{address}"), None);
        let mut params = request();
        params.q = "needle".to_owned();
        client.search(&params).await.unwrap();
        params.sort = SearchSortOrder::OldestFirst;
        params.cursor = Some("oldest-cursor".to_owned());
        client.search(&params).await.unwrap();
        let targets = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        let urls = targets
            .iter()
            .map(|target| reqwest::Url::parse(&format!("http://{address}{target}")).unwrap())
            .collect::<Vec<_>>();
        let params = urls
            .iter()
            .map(|url| {
                url.query_pairs()
                    .into_owned()
                    .collect::<std::collections::HashMap<_, _>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(params[0]["sort"], "newest");
        assert!(!params[0].contains_key("cursor"));
        assert_eq!(params[1]["sort"], "oldest");
        assert_eq!(params[1]["cursor"], "oldest-cursor");
    }

    #[test]
    fn empty_search_requires_a_narrowing_filter() {
        assert!(!request().has_narrowing_filter());

        let mut filtered = request();
        filtered.sender = Some("jamie".to_owned());
        assert!(filtered.has_narrowing_filter());

        filtered.sender = None;
        filtered.room_id = Some("!room:example.org".to_owned());
        assert!(filtered.has_narrowing_filter());
    }

    #[test]
    fn filter_only_search_status_names_filtered_messages() {
        assert_eq!(
            searching_status(&request()),
            "searching filtered messages..."
        );

        let mut text = request();
        text.q = "backup".to_owned();
        assert_eq!(searching_status(&text), "searching for \"backup\"...");
    }
}
